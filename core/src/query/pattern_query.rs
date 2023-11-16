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

use std::sync::Arc;

use optics::PatternPart;
use tantivy::{
    fieldnorm::FieldNormReader,
    postings::SegmentPostings,
    query::{EmptyScorer, Explanation, Scorer},
    schema::IndexRecordOption,
    tokenizer::Tokenizer,
    DocId, DocSet, Postings, Score, SegmentReader, TantivyError, TERMINATED,
};

use crate::{
    fastfield_reader::{self, FastFieldReader},
    query::intersection::Intersection,
    ranking::bm25::Bm25Weight,
    schema::{FastField, Field, TextField, ALL_FIELDS},
};

#[derive(Clone)]
pub struct PatternQuery {
    patterns: Vec<PatternPart>,
    can_optimize_site_domain: bool,
    field: tantivy::schema::Field,
    raw_terms: Vec<tantivy::Term>,
    fastfield_reader: FastFieldReader,
}

impl std::fmt::Debug for PatternQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PatternQuery")
            .field("patterns", &self.patterns)
            .field("field", &self.field)
            .field("raw_terms", &self.raw_terms)
            .finish()
    }
}

impl PatternQuery {
    pub fn new(
        patterns: Vec<PatternPart>,
        field: TextField,
        schema: &tantivy::schema::Schema,
        fastfield_reader: FastFieldReader,
    ) -> Self {
        let mut field = Field::Text(field);
        let mut tv_field = schema.get_field(field.name()).unwrap();

        if can_optimize_site_domain(&patterns, field) {
            if patterns.len() == 3 {
                let PatternPart::Raw(term) = &patterns[1] else {
                    unreachable!()
                };

                if let Field::Text(TextField::UrlForSiteOperator) = field {
                    field = Field::Text(TextField::SiteWithout);
                    tv_field = schema.get_field(field.name()).unwrap();
                }

                return Self {
                    patterns: Vec::new(),
                    field: tv_field,
                    can_optimize_site_domain: true,
                    raw_terms: vec![tantivy::Term::from_field_text(tv_field, term.as_str())],
                    fastfield_reader,
                };
            } else {
                let term: String = patterns
                    .iter()
                    .filter_map(|p| match p {
                        PatternPart::Raw(s) => Some(s.clone()),
                        PatternPart::Wildcard => None,
                        PatternPart::Anchor => None,
                    })
                    .collect();

                return Self {
                    patterns,
                    field: tv_field,
                    can_optimize_site_domain: true,
                    raw_terms: vec![tantivy::Term::from_field_text(tv_field, &term)],
                    fastfield_reader,
                };
            }
        }

        let mut raw_terms = Vec::with_capacity(patterns.len());
        let mut new_patterns = Vec::with_capacity(patterns.len());

        for pattern in &patterns {
            match pattern {
                PatternPart::Raw(text) => {
                    let mut tokenizer = field.as_text().unwrap().indexing_tokenizer();
                    let mut stream = tokenizer.token_stream(text);

                    while let Some(token) = stream.next() {
                        new_patterns.push(PatternPart::Raw(token.text.clone()));
                        let term = tantivy::Term::from_field_text(tv_field, &token.text);
                        raw_terms.push(term);
                    }
                }
                PatternPart::Wildcard => new_patterns.push(PatternPart::Wildcard),
                PatternPart::Anchor => new_patterns.push(PatternPart::Anchor),
            }
        }

        raw_terms.shrink_to_fit();

        Self {
            patterns: new_patterns,
            field: tv_field,
            raw_terms,
            can_optimize_site_domain: false,
            fastfield_reader,
        }
    }
}

impl tantivy::query::Query for PatternQuery {
    fn weight(
        &self,
        scoring: tantivy::query::EnableScoring<'_>,
    ) -> tantivy::Result<Box<dyn tantivy::query::Weight>> {
        let bm25_weight = match scoring {
            tantivy::query::EnableScoring::Enabled {
                searcher,
                statistics_provider: _,
            } => {
                if self.raw_terms.is_empty() {
                    None
                } else {
                    Some(Bm25Weight::for_terms(searcher, &self.raw_terms)?)
                }
            }
            tantivy::query::EnableScoring::Disabled { .. } => None,
        };

        if self.can_optimize_site_domain {
            return Ok(Box::new(FastSiteDomainPatternWeight {
                term: self.raw_terms[0].clone(),
                field: self.field,
                similarity_weight: bm25_weight,
            }));
        }

        Ok(Box::new(PatternWeight {
            similarity_weight: bm25_weight,
            raw_terms: self.raw_terms.clone(),
            patterns: self.patterns.clone(),
            field: self.field,
            fastfield_reader: self.fastfield_reader.clone(),
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a tantivy::Term, bool)) {
        for term in &self.raw_terms {
            visitor(term, true);
        }
    }
}

#[derive(Debug)]
enum SmallPatternPart {
    Term,
    Wildcard,
    Anchor,
}

/// if pattern is of form Site("|site|") or Domain("|domain|")
/// we can use the field without tokenization to speed up the query significantly
fn can_optimize_site_domain(patterns: &[PatternPart], field: Field) -> bool {
    patterns.len() >= 2
        && matches!(&patterns[0], PatternPart::Anchor)
        && matches!(&patterns[patterns.len() - 1], PatternPart::Anchor)
        && patterns[1..patterns.len() - 1]
            .iter()
            .all(|pattern| matches!(pattern, PatternPart::Raw(_)))
        && (matches!(field, Field::Text(TextField::UrlForSiteOperator))
            || matches!(field, Field::Text(TextField::Domain)))
}

struct FastSiteDomainPatternWeight {
    term: tantivy::Term,
    field: tantivy::schema::Field,
    similarity_weight: Option<Bm25Weight>,
}

impl FastSiteDomainPatternWeight {
    fn fieldnorm_reader(&self, reader: &SegmentReader) -> tantivy::Result<FieldNormReader> {
        if self.similarity_weight.is_some() {
            if let Some(fieldnorm_reader) = reader.fieldnorms_readers().get_field(self.field)? {
                return Ok(fieldnorm_reader);
            }
        }
        Ok(FieldNormReader::constant(reader.max_doc(), 1))
    }

    fn pattern_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> tantivy::Result<Option<FastSiteDomainPatternScorer>> {
        let similarity_weight = self
            .similarity_weight
            .as_ref()
            .map(|weight| weight.boost_by(boost));

        let fieldnorm_reader = self.fieldnorm_reader(reader)?;

        let field_no_tokenizer = match ALL_FIELDS[self.field.field_id() as usize] {
            Field::Text(TextField::SiteWithout) => Field::Text(TextField::SiteNoTokenizer),
            Field::Text(TextField::Domain) => Field::Text(TextField::DomainNoTokenizer),
            _ => unreachable!(),
        };

        let tv_field = reader
            .schema()
            .get_field(field_no_tokenizer.name())
            .unwrap();

        let opt = match field_no_tokenizer {
            Field::Text(t) => t.index_option(),
            Field::Fast(_) => unreachable!(),
        };

        match reader
            .inverted_index(tv_field)?
            .read_postings(&self.term, opt)?
        {
            Some(posting) => Ok(Some(FastSiteDomainPatternScorer {
                similarity_weight,
                posting,
                fieldnorm_reader,
            })),
            None => Ok(None),
        }
    }
}

impl tantivy::query::Weight for FastSiteDomainPatternWeight {
    fn scorer(
        &self,
        reader: &tantivy::SegmentReader,
        boost: tantivy::Score,
    ) -> tantivy::Result<Box<dyn tantivy::query::Scorer>> {
        if let Some(scorer) = self.pattern_scorer(reader, boost)? {
            Ok(Box::new(PatternScorer::FastSiteDomain(scorer)))
        } else {
            Ok(Box::new(EmptyScorer))
        }
    }

    fn explain(
        &self,
        reader: &tantivy::SegmentReader,
        doc: tantivy::DocId,
    ) -> tantivy::Result<tantivy::query::Explanation> {
        let scorer_opt = self.pattern_scorer(reader, 1.0)?;
        if scorer_opt.is_none() {
            return Err(TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match (empty scorer)"
            )));
        }
        let mut scorer = scorer_opt.unwrap();
        if scorer.seek(doc) != doc {
            return Err(TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match"
            )));
        }
        let fieldnorm_reader = self.fieldnorm_reader(reader)?;
        let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc);
        let term_freq = scorer.term_freq();
        let mut explanation = Explanation::new("Pattern Scorer", scorer.score());
        explanation.add_detail(
            self.similarity_weight
                .as_ref()
                .unwrap()
                .explain(fieldnorm_id, term_freq),
        );
        Ok(explanation)
    }
}

struct PatternWeight {
    similarity_weight: Option<Bm25Weight>,
    patterns: Vec<PatternPart>,
    raw_terms: Vec<tantivy::Term>,
    field: tantivy::schema::Field,
    fastfield_reader: FastFieldReader,
}

impl PatternWeight {
    fn fieldnorm_reader(&self, reader: &SegmentReader) -> tantivy::Result<FieldNormReader> {
        if self.similarity_weight.is_some() {
            if let Some(fieldnorm_reader) = reader.fieldnorms_readers().get_field(self.field)? {
                return Ok(fieldnorm_reader);
            }
        }
        Ok(FieldNormReader::constant(reader.max_doc(), 1))
    }

    pub(crate) fn pattern_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> tantivy::Result<Option<PatternScorer>> {
        if self.patterns.is_empty() {
            return Ok(None);
        }

        let num_tokens_fastfield = match &ALL_FIELDS[self.field.field_id() as usize] {
            Field::Text(TextField::Title) => Ok(FastField::NumTitleTokens),
            Field::Text(TextField::CleanBody) => Ok(FastField::NumCleanBodyTokens),
            Field::Text(TextField::Url) => Ok(FastField::NumUrlTokens),
            Field::Text(TextField::Domain) => Ok(FastField::NumDomainTokens),
            Field::Text(TextField::UrlForSiteOperator) => {
                Ok(FastField::NumUrlForSiteOperatorTokens)
            }
            Field::Text(TextField::Description) => Ok(FastField::NumDescriptionTokens),
            Field::Text(TextField::MicroformatTags) => Ok(FastField::NumMicroformatTagsTokens),
            Field::Text(TextField::FlattenedSchemaOrgJson) => {
                Ok(FastField::NumFlattenedSchemaTokens)
            }
            field => Err(TantivyError::InvalidArgument(format!(
                "{} is not supported in pattern query",
                field.name()
            ))),
        }?;

        // "*" matches everything
        if self.raw_terms.is_empty()
            && self
                .patterns
                .iter()
                .any(|p| matches!(p, PatternPart::Wildcard))
        {
            return Ok(Some(PatternScorer::Everything(AllScorer {
                doc: 0,
                max_doc: reader.max_doc(),
            })));
        }

        // "||" and "|" matches empty string

        if self.raw_terms.is_empty()
            && self
                .patterns
                .iter()
                .all(|p| matches!(p, PatternPart::Anchor))
        {
            return Ok(Some(PatternScorer::EmptyField(EmptyFieldScorer {
                num_tokens_fastfield,
                segment_reader: self.fastfield_reader.get_segment(&reader.segment_id()),
                all_scorer: AllScorer {
                    doc: 0,
                    max_doc: reader.max_doc(),
                },
            })));
        }

        let similarity_weight = self
            .similarity_weight
            .as_ref()
            .map(|weight| weight.boost_by(boost));

        let fieldnorm_reader = self.fieldnorm_reader(reader)?;

        let mut term_postings_list = Vec::with_capacity(self.raw_terms.len());
        for term in &self.raw_terms {
            if let Some(postings) = reader
                .inverted_index(term.field())?
                .read_postings(term, IndexRecordOption::WithFreqsAndPositions)?
            {
                term_postings_list.push(postings);
            } else {
                return Ok(None);
            }
        }

        let small_patterns = self
            .patterns
            .iter()
            .map(|pattern| match pattern {
                PatternPart::Raw(_) => SmallPatternPart::Term,
                PatternPart::Wildcard => SmallPatternPart::Wildcard,
                PatternPart::Anchor => SmallPatternPart::Anchor,
            })
            .collect();

        Ok(Some(PatternScorer::Normal(NormalPatternScorer::new(
            similarity_weight,
            term_postings_list,
            fieldnorm_reader,
            small_patterns,
            reader.segment_id(),
            num_tokens_fastfield,
            self.fastfield_reader.clone(),
        ))))
    }
}

impl tantivy::query::Weight for PatternWeight {
    fn scorer(
        &self,
        reader: &tantivy::SegmentReader,
        boost: tantivy::Score,
    ) -> tantivy::Result<Box<dyn tantivy::query::Scorer>> {
        if let Some(scorer) = self.pattern_scorer(reader, boost)? {
            Ok(Box::new(scorer))
        } else {
            Ok(Box::new(EmptyScorer))
        }
    }

    fn explain(
        &self,
        reader: &tantivy::SegmentReader,
        doc: tantivy::DocId,
    ) -> tantivy::Result<tantivy::query::Explanation> {
        let scorer_opt = self.pattern_scorer(reader, 1.0)?;
        if scorer_opt.is_none() {
            return Err(TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match (empty scorer)"
            )));
        }
        let mut scorer = scorer_opt.unwrap();
        if scorer.seek(doc) != doc {
            return Err(TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match"
            )));
        }
        let fieldnorm_reader = self.fieldnorm_reader(reader)?;
        let fieldnorm_id = fieldnorm_reader.fieldnorm_id(doc);
        let term_freq = scorer.term_freq();
        let mut explanation = Explanation::new("Pattern Scorer", scorer.score());
        explanation.add_detail(
            self.similarity_weight
                .as_ref()
                .unwrap()
                .explain(fieldnorm_id, term_freq),
        );
        Ok(explanation)
    }
}

enum PatternScorer {
    Normal(NormalPatternScorer),
    FastSiteDomain(FastSiteDomainPatternScorer),
    Everything(AllScorer),
    EmptyField(EmptyFieldScorer),
}

impl Scorer for PatternScorer {
    fn score(&mut self) -> Score {
        match self {
            PatternScorer::Normal(scorer) => scorer.score(),
            PatternScorer::FastSiteDomain(scorer) => scorer.score(),
            PatternScorer::Everything(scorer) => scorer.score(),
            PatternScorer::EmptyField(scorer) => scorer.score(),
        }
    }
}

impl DocSet for PatternScorer {
    fn advance(&mut self) -> DocId {
        match self {
            PatternScorer::Normal(scorer) => scorer.advance(),
            PatternScorer::FastSiteDomain(scorer) => scorer.advance(),
            PatternScorer::Everything(scorer) => scorer.advance(),
            PatternScorer::EmptyField(scorer) => scorer.advance(),
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        match self {
            PatternScorer::Normal(scorer) => scorer.seek(target),
            PatternScorer::FastSiteDomain(scorer) => scorer.seek(target),
            PatternScorer::Everything(scorer) => scorer.seek(target),
            PatternScorer::EmptyField(scorer) => scorer.seek(target),
        }
    }

    fn doc(&self) -> DocId {
        match self {
            PatternScorer::Normal(scorer) => scorer.doc(),
            PatternScorer::FastSiteDomain(scorer) => scorer.doc(),
            PatternScorer::Everything(scorer) => scorer.doc(),
            PatternScorer::EmptyField(scorer) => scorer.doc(),
        }
    }

    fn size_hint(&self) -> u32 {
        match self {
            PatternScorer::Normal(scorer) => scorer.size_hint(),
            PatternScorer::FastSiteDomain(scorer) => scorer.size_hint(),
            PatternScorer::Everything(scorer) => scorer.size_hint(),
            PatternScorer::EmptyField(scorer) => scorer.size_hint(),
        }
    }
}

impl PatternScorer {
    fn term_freq(&self) -> u32 {
        match self {
            PatternScorer::Normal(scorer) => scorer.phrase_count(),
            PatternScorer::FastSiteDomain(scorer) => scorer.term_freq(),
            PatternScorer::Everything(_) => 0,
            PatternScorer::EmptyField(_) => 0,
        }
    }
}

struct AllScorer {
    doc: DocId,
    max_doc: DocId,
}

impl DocSet for AllScorer {
    fn advance(&mut self) -> DocId {
        if self.doc + 1 >= self.max_doc {
            self.doc = TERMINATED;
            return TERMINATED;
        }
        self.doc += 1;
        self.doc
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if target >= self.max_doc {
            self.doc = TERMINATED;
            return TERMINATED;
        }
        self.doc = target;
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        self.max_doc
    }
}

impl Scorer for AllScorer {
    fn score(&mut self) -> Score {
        1.0
    }
}

struct EmptyFieldScorer {
    segment_reader: Arc<fastfield_reader::SegmentReader>,
    num_tokens_fastfield: FastField,
    all_scorer: AllScorer,
}

impl EmptyFieldScorer {
    fn num_tokes(&self, doc: DocId) -> u64 {
        let s: Option<u64> = self
            .segment_reader
            .get_field_reader(&self.num_tokens_fastfield)
            .get(&doc)
            .into();
        s.unwrap_or_default()
    }
}

impl DocSet for EmptyFieldScorer {
    fn advance(&mut self) -> DocId {
        let mut doc = self.all_scorer.advance();

        while doc != TERMINATED && self.num_tokes(doc) > 0 {
            doc = self.all_scorer.advance();
        }

        doc
    }

    fn doc(&self) -> DocId {
        self.all_scorer.doc()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.all_scorer.seek(target);

        if self.doc() != TERMINATED && self.num_tokes(self.all_scorer.doc()) > 0 {
            self.advance()
        } else {
            self.doc()
        }
    }

    fn size_hint(&self) -> u32 {
        self.all_scorer.size_hint()
    }
}

impl Scorer for EmptyFieldScorer {
    fn score(&mut self) -> Score {
        1.0
    }
}

struct FastSiteDomainPatternScorer {
    similarity_weight: Option<Bm25Weight>,
    posting: SegmentPostings,
    fieldnorm_reader: FieldNormReader,
}
impl FastSiteDomainPatternScorer {
    fn term_freq(&self) -> u32 {
        self.posting.term_freq()
    }
}

impl Scorer for FastSiteDomainPatternScorer {
    fn score(&mut self) -> Score {
        self.similarity_weight
            .as_ref()
            .map(|w| {
                w.score(
                    self.fieldnorm_reader.fieldnorm_id(self.doc()),
                    self.posting.term_freq(),
                )
            })
            .unwrap_or_default()
    }
}

impl DocSet for FastSiteDomainPatternScorer {
    fn advance(&mut self) -> DocId {
        self.posting.advance()
    }

    fn doc(&self) -> DocId {
        self.posting.doc()
    }

    fn size_hint(&self) -> u32 {
        self.posting.size_hint()
    }
}

struct NormalPatternScorer {
    pattern_all_simple: bool,
    similarity_weight: Option<Bm25Weight>,
    fieldnorm_reader: FieldNormReader,
    intersection_docset: Intersection<SegmentPostings>,
    pattern: Vec<SmallPatternPart>,
    num_query_terms: usize,
    left: Vec<u32>,
    right: Vec<u32>,
    phrase_count: u32,
    num_tokens_field: FastField,
    segment_reader: Arc<fastfield_reader::SegmentReader>,
}

impl NormalPatternScorer {
    fn new(
        similarity_weight: Option<Bm25Weight>,
        term_postings_list: Vec<SegmentPostings>,
        fieldnorm_reader: FieldNormReader,
        pattern: Vec<SmallPatternPart>,
        segment: tantivy::SegmentId,
        num_tokens_field: FastField,
        fastfield_reader: FastFieldReader,
    ) -> Self {
        let num_query_terms = term_postings_list.len();
        let segment_reader = fastfield_reader.get_segment(&segment);

        let mut s = Self {
            pattern_all_simple: pattern.iter().all(|p| matches!(p, SmallPatternPart::Term)),
            intersection_docset: Intersection::new(term_postings_list),
            num_query_terms,
            similarity_weight,
            fieldnorm_reader,
            pattern,
            left: Vec::with_capacity(100),
            right: Vec::with_capacity(100),
            phrase_count: 0,
            num_tokens_field,
            segment_reader,
        };

        if !s.pattern_match() {
            s.advance();
        }

        s
    }
    fn phrase_count(&self) -> u32 {
        self.phrase_count
    }

    fn pattern_match(&mut self) -> bool {
        if self.num_query_terms == 1 && self.pattern_all_simple {
            // speedup for single term patterns
            self.phrase_count = self
                .intersection_docset
                .docset_mut_specialized(0)
                .term_freq();
            return self.phrase_count > 0;
        }

        self.phrase_count = self.perform_pattern_match() as u32;

        self.phrase_count > 0
    }

    fn perform_pattern_match(&mut self) -> usize {
        if self.intersection_docset.doc() == TERMINATED {
            return 0;
        }

        {
            self.intersection_docset
                .docset_mut_specialized(0)
                .positions(&mut self.left);
        }

        let mut intersection_len = self.left.len();
        let mut out = Vec::new();

        let mut current_right_term = 0;
        let mut slop = 1;
        let num_tokens_doc: Option<u64> = self
            .segment_reader
            .get_field_reader(&self.num_tokens_field)
            .get(&self.doc())
            .into();
        let num_tokens_doc = num_tokens_doc.unwrap();

        for (i, pattern_part) in self.pattern.iter().enumerate() {
            match pattern_part {
                SmallPatternPart::Term => {
                    if current_right_term == 0 {
                        current_right_term = 1;
                        continue;
                    }

                    {
                        self.intersection_docset
                            .docset_mut_specialized(current_right_term)
                            .positions(&mut self.right);
                    }
                    out.resize(self.left.len().max(self.right.len()), 0);
                    intersection_len =
                        intersection_with_slop(&self.left[..], &self.right[..], &mut out, slop);

                    slop = 1;

                    if intersection_len == 0 {
                        return 0;
                    }

                    self.left = out[..intersection_len].to_vec();
                    out = Vec::new();
                    current_right_term += 1;
                }
                SmallPatternPart::Wildcard => {
                    slop = u32::MAX;
                }
                SmallPatternPart::Anchor if i == 0 => {
                    if let Some(pos) = self.left.first() {
                        if *pos != 0 {
                            return 0;
                        }
                    }
                }
                SmallPatternPart::Anchor if i == self.pattern.len() - 1 => {
                    {
                        self.intersection_docset
                            .docset_mut_specialized(self.num_query_terms - 1)
                            .positions(&mut self.right);
                    }

                    if let Some(pos) = self.right.last() {
                        if *pos != (num_tokens_doc - 1) as u32 {
                            return 0;
                        }
                    }
                }
                SmallPatternPart::Anchor => {}
            }
        }

        intersection_len
    }
}

impl Scorer for NormalPatternScorer {
    fn score(&mut self) -> Score {
        self.similarity_weight
            .as_ref()
            .map(|scorer| {
                let doc = self.doc();
                let fieldnorm_id = self.fieldnorm_reader.fieldnorm_id(doc);
                scorer.score(fieldnorm_id, self.phrase_count())
            })
            .unwrap_or_default()
    }
}

impl DocSet for NormalPatternScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.intersection_docset.advance();
            if doc == TERMINATED || self.pattern_match() {
                return doc;
            }
        }
    }

    fn doc(&self) -> tantivy::DocId {
        self.intersection_docset.doc()
    }

    fn size_hint(&self) -> u32 {
        self.intersection_docset.size_hint()
    }
}

/// Intersect twos sorted arrays `left` and `right` and outputs the
/// resulting array in `out`. The positions in out are all positions from right where
/// the distance to left_pos <= slop
///
/// Returns the length of the intersection
fn intersection_with_slop(left: &[u32], right: &[u32], out: &mut [u32], slop: u32) -> usize {
    let mut left_index = 0;
    let mut right_index = 0;
    let mut count = 0;
    let left_len = left.len();
    let right_len = right.len();
    while left_index < left_len && right_index < right_len {
        let left_val = left[left_index];
        let right_val = right[right_index];

        // The three conditions are:
        // left_val < right_slop -> left index increment.
        // right_slop <= left_val <= right -> find the best match.
        // left_val > right -> right index increment.
        let right_slop = if right_val >= slop {
            right_val - slop
        } else {
            0
        };

        if left_val < right_slop {
            left_index += 1;
        } else if right_slop <= left_val && left_val <= right_val {
            while left_index + 1 < left_len {
                // there could be a better match
                let next_left_val = left[left_index + 1];
                if next_left_val > right_val {
                    // the next value is outside the range, so current one is the best.
                    break;
                }
                // the next value is better.
                left_index += 1;
            }
            // store the match in left.
            out[count] = right_val;
            count += 1;
            right_index += 1;
        } else if left_val > right_val {
            right_index += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aux_intersection(left: &[u32], right: &[u32], expected: &[u32], slop: u32) {
        let mut out = vec![0; left.len().max(right.len())];

        let intersection_size = intersection_with_slop(left, right, &mut out, slop);

        assert_eq!(&out[..intersection_size], expected);
    }

    #[test]
    fn test_intersection_with_slop() {
        aux_intersection(&[20, 75, 77], &[18, 21, 60], &[21, 60], u32::MAX);
        aux_intersection(&[21, 60], &[50, 61], &[61], 1);

        aux_intersection(&[1, 2, 3], &[], &[], 1);
        aux_intersection(&[], &[1, 2, 3], &[], 1);

        aux_intersection(&[1, 2, 3], &[4, 5, 6], &[4], 1);
        aux_intersection(&[1, 2, 3], &[4, 5, 6], &[4, 5, 6], u32::MAX);

        aux_intersection(&[20, 75, 77], &[18, 21, 60], &[21, 60], u32::MAX);
        aux_intersection(&[21, 60], &[61, 62], &[61, 62], 2);

        aux_intersection(&[60], &[61, 62], &[61, 62], 2);
    }
}
