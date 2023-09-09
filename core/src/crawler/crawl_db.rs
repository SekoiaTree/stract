// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use dashmap::DashMap;
use hashbrown::{HashMap, HashSet};
use rand::Rng;
use rayon::prelude::*;
use std::hash::Hash;
use std::ops::Range;
use std::path::PathBuf;
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, VecDeque},
    path::Path,
};
use url::Url;

use super::{Domain, Job, JobResponse, Result, UrlResponse};

const MAX_URL_DB_SIZE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10GB

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UrlStatus {
    Pending,
    Crawling,
    Failed { status_code: Option<u16> },
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DomainStatus {
    Pending,
    NoUncrawledUrls,
    CrawlInProgress,
}

struct SampledItem<T> {
    item: T,
    priority: f64,
}

impl<T> PartialEq for SampledItem<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl<T> Eq for SampledItem<T> {}

impl<T> PartialOrd for SampledItem<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for SampledItem<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.total_cmp(&other.priority)
    }
}

fn weighted_sample<T>(items: impl Iterator<Item = (T, f64)>, num_items: usize) -> Vec<T> {
    let mut sampled_items: BinaryHeap<SampledItem<T>> = BinaryHeap::with_capacity(num_items);

    let mut rng = rand::thread_rng();

    for (item, weight) in items {
        // see https://www.kaggle.com/code/kotamori/random-sample-with-weights-on-sql/notebook for details on math
        let priority = -(rng.gen::<f64>().abs() + f64::EPSILON).ln() / (weight + 1.0);

        if sampled_items.len() < num_items {
            sampled_items.push(SampledItem { item, priority });
        } else if let Some(mut max) = sampled_items.peek_mut() {
            if priority < max.priority {
                max.item = item;
                max.priority = priority;
            }
        }
    }

    sampled_items.into_iter().map(|s| s.item).collect()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct UrlState {
    weight: f64,
    status: UrlStatus,
}

impl Default for UrlState {
    fn default() -> Self {
        Self {
            weight: 0.0,
            status: UrlStatus::Pending,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DomainState {
    weight: f64,
    status: DomainStatus,
    total_urls: u64,
}

impl Default for DomainState {
    fn default() -> Self {
        Self {
            weight: 0.0,
            status: DomainStatus::Pending,
            total_urls: 0,
        }
    }
}

pub struct RedirectDb {
    inner: rocksdb::DB,
}

impl RedirectDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();

        options.create_if_missing(true);

        let mut block_options = rocksdb::BlockBasedOptions::default();
        block_options.set_format_version(5);

        options.set_block_based_table_factory(&block_options);

        let inner = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { inner })
    }

    pub fn put(&self, from: &Url, to: &Url) -> Result<()> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = bincode::serialize(to)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);
        self.inner
            .put_opt(url_bytes, redirect_bytes, &write_options)?;

        Ok(())
    }

    pub fn get(&self, from: &Url) -> Result<Option<Url>> {
        let url_bytes = bincode::serialize(from)?;
        let redirect_bytes = self.inner.get(url_bytes)?;

        if let Some(redirect_bytes) = redirect_bytes {
            let redirect: Url = bincode::deserialize(&redirect_bytes)?;
            return Ok(Some(redirect));
        }

        Ok(None)
    }
}

struct RangesDb {
    db: rocksdb::DB, // domain -> range<vec<u8>>
    /// from rocksdb docs: "Cache must outlive DB instance which uses it."
    _cache: rocksdb::Cache,
}

impl RangesDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();
        let cache = rocksdb::Cache::new_lru_cache(100 * 1024 * 1024)?; // 100MB

        options.create_if_missing(true);
        options.set_row_cache(&cache);

        let mut block_options = rocksdb::BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);
        block_options.set_format_version(5);

        options.set_block_based_table_factory(&block_options);
        options.set_max_background_jobs(8);
        options.increase_parallelism(8);
        options.set_write_buffer_size(512 * 1024 * 1024);
        options.set_allow_mmap_reads(true);
        options.set_allow_mmap_writes(true);
        options.set_max_subcompactions(8);
        options.optimize_for_point_lookup(512);
        options.set_compression_type(rocksdb::DBCompressionType::None);

        let db = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { db, _cache: cache })
    }

    pub fn get(&self, domain: &Domain) -> Result<Option<Range<Vec<u8>>>> {
        let domain_bytes = bincode::serialize(domain)?;
        let range_bytes = self.db.get(domain_bytes)?;

        if let Some(range_bytes) = range_bytes {
            let range: Range<Vec<u8>> = bincode::deserialize(&range_bytes)?;
            return Ok(Some(range));
        }

        Ok(None)
    }

    pub fn put(&self, domain: &Domain, range: &Range<Vec<u8>>) -> Result<()> {
        let domain_bytes = bincode::serialize(domain)?;
        let range_bytes = bincode::serialize(range)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);
        self.db.put_opt(domain_bytes, range_bytes, &write_options)?;

        Ok(())
    }
}

struct CachedValue<T> {
    value: T,
    last_updated: std::time::Instant,
}

impl<T> From<T> for CachedValue<T> {
    fn from(value: T) -> Self {
        Self {
            value,
            last_updated: std::time::Instant::now(),
        }
    }
}

struct UrlToInsert {
    url: Url,
    different_domain: bool,
}

struct UrlStateDbShard {
    db: rocksdb::DB,
    ranges: RangesDb,
    /// from rocksdb docs: "Cache must outlive DB instance which uses it."
    _cache: rocksdb::Cache,
    approx_size_bytes: CachedValue<u64>,
}

impl UrlStateDbShard {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();

        options.create_if_missing(true);

        let mut block_options = rocksdb::BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);
        block_options.set_format_version(5);

        let cache = rocksdb::Cache::new_lru_cache(1024 * 1024 * 1024)?; // 1GB
        block_options.set_block_cache(&cache);

        options.set_block_based_table_factory(&block_options);
        options.set_max_background_jobs(8);
        options.increase_parallelism(8);
        options.set_write_buffer_size(512 * 1024 * 1024);
        options.set_allow_mmap_reads(true);
        options.set_allow_mmap_writes(true);
        options.set_max_subcompactions(8);
        options.set_compaction_style(rocksdb::DBCompactionStyle::Universal);
        options.set_compression_type(rocksdb::DBCompressionType::None);

        let db = rocksdb::DB::open(&options, path.as_ref().join("urls"))?;
        let approx_size_bytes = db
            .property_int_value(rocksdb::properties::TOTAL_SST_FILES_SIZE)?
            .unwrap()
            .into();

        Ok(Self {
            db,
            approx_size_bytes,
            _cache: cache,
            ranges: RangesDb::open(path.as_ref().join("ranges"))?,
        })
    }

    pub fn get(&self, domain: &Domain, url: &UrlString) -> Result<Option<UrlState>> {
        let url_bytes = bincode::serialize(url)?;
        let domain_bytes = bincode::serialize(domain)?;

        let key_bytes = [domain_bytes.as_slice(), &[b'/'], url_bytes.as_slice()].concat();

        let state_bytes = self.db.get(key_bytes)?;

        match state_bytes {
            Some(state_bytes) => {
                let state = bincode::deserialize(&state_bytes).unwrap();
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    pub fn put_batch(&mut self, domain: &Domain, urls: &[(UrlString, UrlState)]) -> Result<()> {
        let mut range = self.ranges.get(domain)?;

        let domain_bytes = bincode::serialize(domain)?;

        let mut batch = rocksdb::WriteBatch::default();

        for (url, state) in urls {
            let url_bytes = bincode::serialize(url)?;

            let key_bytes = [domain_bytes.as_slice(), &[b'/'], url_bytes.as_slice()].concat();

            // update ranges

            if range.is_none() {
                range = Some(Range {
                    start: key_bytes.clone(),
                    end: key_bytes.clone(),
                });
            }

            let range = range.as_mut().unwrap();

            if key_bytes < range.start {
                range.start = key_bytes.clone();
            }

            if key_bytes > range.end {
                range.end = key_bytes.clone();
            }
            let state_bytes = bincode::serialize(state)?;

            batch.put(key_bytes, state_bytes);
        }

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);

        self.db.write_opt(batch, &write_options)?;

        if let Some(range) = range {
            self.ranges.put(domain, &range)?;
        }

        Ok(())
    }

    pub fn get_all_urls(&self, domain: &Domain) -> Result<Vec<(UrlString, UrlState)>> {
        match self.ranges.get(domain)? {
            Some(range) => {
                let domain_bytes = bincode::serialize(domain)?;
                let start = range.start.clone();
                let end = range.end.clone();

                let iter = self.db.iterator(rocksdb::IteratorMode::From(
                    &start,
                    rocksdb::Direction::Forward,
                ));

                Ok(iter
                    .take_while(|r| {
                        if let Ok((key, _)) = r.as_ref() {
                            &**key <= end.as_slice()
                        } else {
                            false
                        }
                    })
                    .filter_map(|r| {
                        let (key, value) = r.ok()?;

                        let url = bincode::deserialize(&key[domain_bytes.len() + 1..]) // +1 for '/'
                            .ok()?;

                        let state = bincode::deserialize(&value[..]).ok()?;

                        Some((url, state))
                    })
                    .collect())
            }
            None => Ok(Vec::new()),
        }
    }

    pub fn approximate_size_bytes(&mut self) -> Result<u64> {
        if self.approx_size_bytes.last_updated.elapsed().as_secs() > 10 {
            self.approx_size_bytes = self
                .db
                .property_int_value(rocksdb::properties::TOTAL_SST_FILES_SIZE)?
                .unwrap_or_default()
                .into();
        }

        Ok(self.approx_size_bytes.value)
    }
}

struct UrlStateDb {
    shards: Vec<UrlStateDbShard>,
    path: PathBuf,
}

impl UrlStateDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if path.as_ref().exists() {
            let mut shard_names = Vec::new();
            for entry in std::fs::read_dir(path.as_ref())? {
                let entry = entry?;
                let path = entry.path();

                if path.is_dir() {
                    shard_names.push(path.to_str().unwrap().to_string());
                }
            }

            shard_names.sort();

            let mut shards = Vec::new();

            for shard_name in shard_names {
                shards.push(UrlStateDbShard::open(shard_name)?);
            }

            Ok(Self {
                shards,
                path: path.as_ref().to_path_buf(),
            })
        } else {
            let shard_id =
                chrono::Utc::now().to_rfc3339() + "_" + uuid::Uuid::new_v4().to_string().as_str();
            let shard_path = path.as_ref().join(shard_id);

            std::fs::create_dir_all(&shard_path)?;

            let shard = UrlStateDbShard::open(&shard_path)?;

            Ok(Self {
                shards: vec![shard],
                path: path.as_ref().to_path_buf(),
            })
        }
    }

    pub fn get(&self, domain: &Domain, url: &UrlString) -> Result<Option<UrlState>> {
        // we iterate in reverse order so that we get the most recent state
        // since we insert new states at the last shard.
        for shard in self.shards.iter().rev() {
            if let Some(state) = shard.get(domain, url)? {
                return Ok(Some(state));
            }
        }

        Ok(None)
    }

    pub fn put_batch(&mut self, domain: &Domain, urls: &[(UrlString, UrlState)]) -> Result<()> {
        let last_shard = self.shards.last_mut().unwrap();

        if last_shard.approximate_size_bytes()? > MAX_URL_DB_SIZE_BYTES {
            let shard_id =
                chrono::Utc::now().to_rfc3339() + "_" + uuid::Uuid::new_v4().to_string().as_str();
            let shard_path = self.path.as_path().join(shard_id);

            std::fs::create_dir_all(&shard_path)?;

            let shard = UrlStateDbShard::open(&shard_path)?;

            self.shards.push(shard);
        }

        self.shards.last_mut().unwrap().put_batch(domain, urls)?;

        Ok(())
    }

    pub fn get_all_urls(&self, domain: &Domain) -> Result<Vec<(UrlString, UrlState)>> {
        let mut res = HashMap::new();

        for shard in &self.shards {
            for (url, state) in shard.get_all_urls(domain)? {
                res.insert(url, state);
            }
        }

        Ok(res.into_iter().collect())
    }
}

struct DomainStateDb {
    db: rocksdb::DB,
}

impl DomainStateDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut options = rocksdb::Options::default();

        options.create_if_missing(true);

        let mut block_options = rocksdb::BlockBasedOptions::default();
        block_options.set_ribbon_filter(10.0);
        block_options.set_format_version(5);

        options.set_block_based_table_factory(&block_options);
        options.set_optimize_filters_for_hits(true);
        options.set_max_background_jobs(8);
        options.increase_parallelism(8);
        options.set_write_buffer_size(512 * 1024 * 1024);
        options.set_allow_mmap_reads(true);
        options.set_allow_mmap_writes(true);
        options.set_max_subcompactions(8);

        let db = rocksdb::DB::open(&options, path.as_ref())?;

        Ok(Self { db })
    }

    fn get(&self, domain: &Domain) -> Result<Option<DomainState>> {
        let domain_bytes = bincode::serialize(&domain)?;
        let value_bytes = self.db.get(domain_bytes)?;

        if let Some(value_bytes) = &value_bytes {
            return Ok(Some(bincode::deserialize(&value_bytes[..])?));
        }

        Ok(None)
    }

    fn put(&self, domain: &Domain, state: &DomainState) -> Result<()> {
        let domain_bytes = bincode::serialize(domain)?;
        let state_bytes = bincode::serialize(state)?;

        let mut write_options = rocksdb::WriteOptions::default();
        write_options.disable_wal(true);
        self.db.put_opt(domain_bytes, state_bytes, &write_options)?;

        Ok(())
    }

    fn iter(&self) -> impl Iterator<Item = (Domain, DomainState)> + '_ {
        let iter = self.db.iterator(rocksdb::IteratorMode::Start);

        iter.filter_map(|r| {
            let (key, value) = r.ok()?;
            let domain = bincode::deserialize(&key[..]).ok()?;
            let state = bincode::deserialize(&value[..]).ok()?;

            Some((domain, state))
        })
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
struct UrlString(String);

impl From<&Url> for UrlString {
    fn from(url: &Url) -> Self {
        Self(url.as_str().to_string())
    }
}

impl From<Url> for UrlString {
    fn from(url: Url) -> Self {
        Self(url.as_str().to_string())
    }
}

impl From<&UrlString> for Url {
    fn from(url: &UrlString) -> Self {
        Url::parse(&url.0).unwrap()
    }
}

pub struct CrawlDb {
    domain_state: DomainStateDb,
    urls: UrlStateDb,
    redirects: RedirectDb,
}

impl CrawlDb {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Ok(Self {
            redirects: RedirectDb::open(path.as_ref().join("redirects"))?,
            domain_state: DomainStateDb::open(path.as_ref().join("domains"))?,
            urls: UrlStateDb::open(path.as_ref().join("urls"))?,
        })
    }

    pub fn insert_seed_urls(&mut self, urls: &[Url]) -> Result<()> {
        for url in urls {
            let domain = Domain::from(url);

            match self.domain_state.get(&domain)? {
                Some(mut state) => {
                    state.total_urls += 1;
                    self.domain_state.put(&domain, &state)?;
                }
                None => self.domain_state.put(
                    &domain,
                    &DomainState {
                        weight: 0.0,
                        status: DomainStatus::Pending,
                        total_urls: 1,
                    },
                )?,
            }

            self.urls
                .put_batch(&domain, &[(UrlString::from(url), UrlState::default())])?;
        }

        Ok(())
    }

    pub fn insert_urls(&mut self, responses: &[JobResponse]) -> Result<HashSet<Domain>> {
        let domains: DashMap<Domain, Vec<UrlToInsert>> = DashMap::new();

        responses.par_iter().for_each(|res| {
            for url in &res.discovered_urls {
                let domain = Domain::from(url);
                let different_domain = res.domain != domain;

                domains.entry(domain).or_default().push(UrlToInsert {
                    url: url.clone(),
                    different_domain,
                });
            }

            for url_res in &res.url_responses {
                if let UrlResponse::Redirected { url, new_url } = url_res {
                    self.redirects.put(url, new_url).ok();
                }
            }
        });

        let mut nonempty_domains = HashSet::new();

        for (domain, urls) in domains.into_iter() {
            let mut domain_state = match self.domain_state.get(&domain)? {
                Some(state) => state,
                None => {
                    let state = DomainState {
                        weight: 0.0,
                        status: DomainStatus::Pending,
                        total_urls: 0,
                    };
                    self.domain_state.put(&domain, &state)?;

                    state
                }
            };

            if !urls.is_empty() {
                nonempty_domains.insert(domain.clone());
            }

            let mut url_states = Vec::new();

            for url in urls {
                let mut url_state = match self.urls.get(&domain, &UrlString::from(&url.url))? {
                    Some(state) => state,
                    None => {
                        domain_state.total_urls += 1;
                        UrlState::default()
                    }
                };

                if url.different_domain {
                    url_state.weight += 1.0;
                }

                if url_state.weight > domain_state.weight {
                    domain_state.weight = url_state.weight;
                }

                url_states.push((UrlString::from(&url.url), url_state));
            }

            self.urls.put_batch(&domain, &url_states)?;

            self.domain_state.put(&domain, &domain_state)?;
        }

        Ok(nonempty_domains)
    }

    pub fn set_domain_status(&mut self, domain: &Domain, status: DomainStatus) -> Result<()> {
        let mut domain_state = self.domain_state.get(domain)?.unwrap_or_default();

        domain_state.status = status;

        self.domain_state.put(domain, &domain_state)?;

        Ok(())
    }

    pub fn sample_domains(&mut self, num_jobs: usize) -> Result<Vec<Domain>> {
        let sampled = weighted_sample(
            self.domain_state.iter().filter_map(|(domain, state)| {
                if state.status == DomainStatus::Pending {
                    Some((domain, state.weight))
                } else {
                    None
                }
            }),
            num_jobs,
        );

        for domain in sampled.iter() {
            let mut state = self.domain_state.get(domain)?.unwrap_or_default();
            state.status = DomainStatus::CrawlInProgress;
            self.domain_state.put(domain, &state)?;
        }

        Ok(sampled)
    }

    pub fn prepare_jobs(&mut self, domains: &[Domain], urls_per_job: usize) -> Result<Vec<Job>> {
        let mut jobs = Vec::with_capacity(domains.len());
        for domain in domains {
            let urls = self.urls.get_all_urls(domain)?;

            let available_urls: Vec<_> = urls
                .iter()
                .filter_map(|(url, state)| {
                    if state.status == UrlStatus::Pending {
                        Some((url.clone(), state.weight))
                    } else {
                        None
                    }
                })
                .collect();

            let sampled: Vec<_> = weighted_sample(
                available_urls.iter().map(|(url, weight)| (url, *weight)),
                urls_per_job,
            );

            let mut new_url_states = Vec::new();

            for url in &sampled {
                let mut state = self.urls.get(domain, url)?.unwrap_or_default();
                state.status = UrlStatus::Crawling;

                new_url_states.push(((*url).clone(), state));
            }

            self.urls.put_batch(domain, &new_url_states)?;

            let mut domain_state = self.domain_state.get(domain)?.unwrap_or_default();

            domain_state.weight = urls
                .iter()
                .filter_map(|(_, state)| {
                    if state.status == UrlStatus::Pending {
                        Some(state.weight)
                    } else {
                        None
                    }
                })
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
                .unwrap_or(0.0);

            self.domain_state.put(domain, &domain_state)?;

            let mut job = Job {
                domain: domain.clone(),
                fetch_sitemap: false, // todo: fetch for new sites
                urls: VecDeque::with_capacity(urls_per_job),
            };

            for url in sampled {
                job.urls.push_back(url.into());
            }

            jobs.push(job);
        }

        Ok(jobs)
    }
}

#[cfg(test)]
mod tests {
    use crate::gen_temp_path;

    use super::*;

    #[test]
    fn sampling() {
        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 10);
        assert_eq!(sampled.len(), items.len());

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);

        let items: Vec<(usize, f64)> = vec![(0, 1.0), (1, 2.0), (2, 3.0), (3, 4.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 0);
        assert_eq!(sampled.len(), 0);

        let items: Vec<(usize, f64)> = vec![(0, 1000000000.0), (1, 2.0)];
        let sampled = weighted_sample(items.iter().map(|(i, w)| (i, *w)), 1);
        assert_eq!(sampled.len(), 1);
        assert_eq!(*sampled[0], 0);
    }

    #[test]
    fn simple_politeness() {
        let mut db = CrawlDb::open(gen_temp_path()).unwrap();

        db.insert_seed_urls(&[Url::parse("https://example.com").unwrap()])
            .unwrap();

        let domain = Domain::from(&Url::parse("https://example.com").unwrap());

        let sample = db.sample_domains(128).unwrap();

        assert_eq!(sample.len(), 1);
        assert_eq!(&sample[0], &domain);
        assert_eq!(
            db.domain_state.get(&domain).unwrap().unwrap().status,
            DomainStatus::CrawlInProgress
        );

        let new_sample = db.sample_domains(128).unwrap();
        assert_eq!(new_sample.len(), 0);
    }

    #[test]
    fn get_all_urls() {
        let mut db = CrawlDb::open(gen_temp_path()).unwrap();

        db.insert_seed_urls(&[
            Url::parse("https://a.com").unwrap(),
            Url::parse("https://b.com").unwrap(),
        ])
        .unwrap();

        let domain = Domain::from(&Url::parse("https://a.com").unwrap());

        let urls = db.urls.get_all_urls(&domain).unwrap();

        assert_eq!(urls.len(), 1);
        assert_eq!(
            urls[0].0,
            UrlString::from(&Url::parse("https://a.com").unwrap())
        );
    }
}
