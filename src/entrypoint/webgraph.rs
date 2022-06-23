// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
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
use crate::{
    mapreduce::{Map, MapReduce, Reduce, Worker},
    warc::WarcFile,
    webgraph::{FrozenWebgraph, Node, Webgraph, WebgraphBuilder},
    webpage::{self, Html},
    HttpConfig, LocalConfig, Result, WarcSource, WebgraphConfig, WebgraphLocalConfig,
    WebgraphMasterConfig,
};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::Path};
use tracing::{debug, info, trace};

#[derive(Debug, Serialize, Deserialize, Clone)]
enum JobConfig {
    Http(HttpConfig),
    Local(LocalConfig),
}

#[derive(Debug, Serialize, Deserialize)]
struct Job {
    config: JobConfig,
    warc_path: String,
    graph_base_path: String,
}

impl Map<FrozenWebgraph> for Job {
    fn map(self) -> FrozenWebgraph {
        let name = self.warc_path.split('/').last().unwrap();

        info!("processing {}", name);

        let mut graph = WebgraphBuilder::new(Path::new(&self.graph_base_path).join(name))
            .with_host_graph()
            .open();

        let source = match self.config {
            JobConfig::Http(config) => WarcSource::HTTP(config),
            JobConfig::Local(config) => WarcSource::Local(config),
        };

        debug!("downlooading warc file");
        let file = WarcFile::download(source, &self.warc_path).unwrap();
        debug!("finished downloading");

        for record in file.records().flatten() {
            let webpage = Html::parse(&record.response.body, &record.request.url);
            for link in webpage
                .links()
                .into_iter()
                .filter(|link| {
                    link.destination.starts_with("http://")
                        || link.destination.starts_with("https://")
                })
                .filter(|link| webpage::domain(&link.source) != webpage::domain(&link.destination))
            {
                trace!("inserting link {:?}", link);
                graph.insert(
                    Node::from(link.source),
                    Node::from(link.destination),
                    link.text,
                );
            }
        }

        info!("{} done", name);

        graph.into()
    }
}

impl Reduce<FrozenWebgraph> for FrozenWebgraph {
    fn reduce(self, other: FrozenWebgraph) -> FrozenWebgraph {
        let mut graph: Webgraph = self.into();

        let other_path = match &other.root {
            crate::directory::DirEntry::Folder { name, entries: _ } => name.clone(),
            crate::directory::DirEntry::File { name, content: _ } => name.clone(),
        };

        let other = other.into();

        graph.merge(other);

        std::fs::remove_dir_all(other_path).unwrap();

        graph.into()
    }
}

pub struct WebgraphEntrypoint {
    config: WebgraphConfig,
    worker_addr: Option<String>,
}

impl WebgraphEntrypoint {
    pub fn new(config: WebgraphConfig, worker_addr: Option<String>) -> Self {
        Self {
            config,
            worker_addr,
        }
    }
    fn run_master(config: &WebgraphMasterConfig) -> Result<()> {
        info!("Running master for webgraph construction");

        let warc_paths = config.warc_source.paths()?;

        let workers: Vec<SocketAddr> = config
            .workers
            .iter()
            .map(|worker| worker.parse().unwrap())
            .collect();

        let job_config = match config.warc_source.clone() {
            WarcSource::S3(_) => todo!("s3 not supported yet"),
            WarcSource::HTTP(config) => JobConfig::Http(config),
            WarcSource::Local(config) => JobConfig::Local(config),
        };

        let mut warc_paths: Box<dyn Iterator<Item = Job> + Send> =
            Box::new(warc_paths.into_iter().map(|warc_path| {
                Job {
                    config: job_config.clone(),
                    warc_path,
                    graph_base_path: config
                        .graph_base_path
                        .clone()
                        .unwrap_or_else(|| "webgraph".to_string()),
                }
            }));

        if let Some(limit) = config.limit_warc_files {
            warc_paths = Box::new(warc_paths.take(limit));
        }

        warc_paths
            .map_reduce(&workers)
            .expect("failed to build webgraph");

        Ok(())
    }

    fn run_worker(worker_addr: String) -> Result<()> {
        Worker::run::<Job, FrozenWebgraph>(
            worker_addr
                .parse::<SocketAddr>()
                .expect("Could not parse worker address"),
        )?;
        Ok(())
    }

    fn run_locally(config: &WebgraphLocalConfig) -> Result<()> {
        let warc_paths = config.warc_source.paths()?;

        let job_config = match config.warc_source.clone() {
            WarcSource::S3(_) => todo!("s3 not supported yet"),
            WarcSource::HTTP(config) => JobConfig::Http(config),
            WarcSource::Local(config) => JobConfig::Local(config),
        };

        warc_paths
            .into_iter()
            .map(|path| Job {
                config: job_config.clone(),
                warc_path: path,
                graph_base_path: "webgraph".to_string(),
            })
            .map(|job| job.map())
            .fold(None, |acc: Option<FrozenWebgraph>, elem| match acc {
                Some(acc) => Some(acc.reduce(elem)),
                None => Some(elem),
            });

        Ok(())
    }

    pub fn run(&self) -> Result<()> {
        match &self.config {
            WebgraphConfig::Master(config) => WebgraphEntrypoint::run_master(config),
            WebgraphConfig::Worker => WebgraphEntrypoint::run_worker(
                self.worker_addr
                    .clone()
                    .expect("Worker address not specified"),
            ),
            WebgraphConfig::Local(config) => WebgraphEntrypoint::run_locally(config),
        }
    }
}
