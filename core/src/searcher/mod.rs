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

pub mod distributed;
pub mod local;

pub use distributed::*;
pub use local::*;
use optics::SiteRankings;
use serde::{Deserialize, Serialize};

use crate::{
    bangs::BangHit,
    ranking::pipeline::RankingWebsite,
    search_prettifier::{
        DisplayedAnswer, DisplayedEntity, DisplayedWebpage, HighlightedSpellCorrection, Sidebar,
    },
    spell::Correction,
    webpage::region::Region,
    widgets::Widget,
};

pub const NUM_RESULTS_PER_PAGE: usize = 20;

#[derive(Debug, Serialize)]
pub enum SearchResult {
    Websites(WebsitesResult),
    Bang(BangHit),
}

#[derive(Debug, Serialize)]
pub struct WebsitesResult {
    pub spell_corrected_query: Option<HighlightedSpellCorrection>,
    pub webpages: Vec<DisplayedWebpage>,
    pub num_hits: usize,
    pub sidebar: Option<Sidebar>,
    pub widget: Option<Widget>,
    pub direct_answer: Option<DisplayedAnswer>,
    pub discussions: Option<Vec<DisplayedWebpage>>,
    pub search_duration_ms: u128,
    pub has_more_results: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SearchQuery {
    pub query: String,
    pub offset: usize,
    pub num_results: usize,
    pub selected_region: Option<Region>,
    pub optic_program: Option<String>,
    pub site_rankings: Option<SiteRankings>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitialWebsiteResult {
    pub spell_corrected_query: Option<Correction>,
    pub num_websites: usize,
    pub websites: Vec<RankingWebsite>,
    pub entity_sidebar: Option<DisplayedEntity>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        // This does not use `..Default::default()` as there should be
        // an explicit compile error when new fields are added to the `SearchQuery` struct
        // to ensure the developer considers what the default should be.
        Self {
            query: Default::default(),
            offset: Default::default(),
            num_results: NUM_RESULTS_PER_PAGE,
            selected_region: Default::default(),
            optic_program: Default::default(),
            site_rankings: Default::default(),
        }
    }
}

impl SearchQuery {
    pub fn is_empty(&self) -> bool {
        self.query.is_empty()
    }
}
