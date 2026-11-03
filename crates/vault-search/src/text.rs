//! The tantivy full-text index (BM25F via per-field boosts).

use crate::query::{parse_query, Clause};
use crate::Result;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED, STRING};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

const TOKENIZER: &str = "vault";
const SCHEMA_VERSION: &str = "2";

#[derive(Debug, Clone, Default)]
pub struct IndexDoc {
    pub file_id: u32,
    pub path: String,
    pub title: String,
    pub aliases: Vec<String>,
    pub headings: Vec<String>,
    pub tags: Vec<String>,
    pub body: String,
    pub mtime_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TextHit {
    pub file_id: u32,
    pub score: f32,
    /// HTML snippet with <b> highlights (body field).
    pub snippet: String,
}

#[derive(Clone, Copy)]
struct Fields {
    title: Field,
    title_prefix: Field,
    aliases: Field,
    headings: Field,
    tags: Field,
    tags_text: Field,
    body: Field,
    path: Field,
    path_prefix: Field,
    file_id: Field,
    mtime: Field,
}

pub struct TextIndex {
    reader: IndexReader,
    writer: parking::Writer,
    f: Fields,
}

mod parking {
    use std::sync::Mutex;
    pub struct Writer(pub Mutex<Option<tantivy::IndexWriter>>);
}

fn schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();
    let text = |positions: bool| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(TOKENIZER)
                .set_index_option(if positions { IndexRecordOption::WithFreqsAndPositions } else { IndexRecordOption::WithFreqs }),
        )
    };
    let f = Fields {
        title: sb.add_text_field("title", text(true) | STORED),
        title_prefix: sb.add_text_field("title_prefix", STRING),
        aliases: sb.add_text_field("aliases", text(true)),
        headings: sb.add_text_field("headings", text(true)),
        tags: sb.add_text_field("tags", STRING),
        tags_text: sb.add_text_field("tags_text", text(false)),
        body: sb.add_text_field("body", text(true) | STORED),
        path: sb.add_text_field("path", STRING | STORED),
        path_prefix: sb.add_text_field("path_prefix", STRING),
        file_id: sb.add_u64_field("file_id", INDEXED | FAST | STORED),
        mtime: sb.add_u64_field("mtime", FAST),
    };
    (sb.build(), f)
}

fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default()).filter(RemoveLongFilter::limit(40)).filter(LowerCaser).filter(AsciiFoldingFilter).build()
}

fn fold(s: &str) -> String {
    let mut a = analyzer();
    let mut ts = a.token_stream(s);
    let mut out = Vec::new();
    while ts.advance() {
        out.push(ts.token().text.clone());
    }
    out.join(" ")
}

impl TextIndex {
    /// Open (or create) the index in `dir`. A schema change wipes and recreates it (it is a cache).
    pub fn open(dir: &Path) -> Result<TextIndex> {
        std::fs::create_dir_all(dir)?;
        let ver_file = dir.join("SCHEMA_VERSION");
        if std::fs::read_to_string(&ver_file).ok().as_deref() != Some(SCHEMA_VERSION) {
            for e in std::fs::read_dir(dir)?.flatten() {
                let _ = if e.path().is_dir() { std::fs::remove_dir_all(e.path()) } else { std::fs::remove_file(e.path()) };
            }
            std::fs::write(&ver_file, SCHEMA_VERSION)?;
        }
        let (schema, f) = schema();
        let mmap = tantivy::directory::MmapDirectory::open(dir)?;
        let index = Index::open_or_create(mmap, schema)?;
        Self::finish(index, f)
    }

    pub fn in_memory() -> Result<TextIndex> {
        let (schema, f) = schema();
        Self::finish(Index::create_in_ram(schema), f)
    }

    fn finish(index: Index, f: Fields) -> Result<TextIndex> {
        index.tokenizers().register(TOKENIZER, analyzer());
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;
        let writer: IndexWriter = index.writer_with_num_threads(std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(2), 256 << 20)?;
        Ok(TextIndex { reader, writer: parking::Writer(std::sync::Mutex::new(Some(writer))), f })
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Stage an upsert. Visible after `commit`.
    pub fn upsert(&self, d: &IndexDoc) -> Result<()> {
        let g = self.writer.0.lock().unwrap();
        let Some(w) = g.as_ref() else { return Ok(()) };
        w.delete_term(Term::from_field_u64(self.f.file_id, d.file_id as u64));
        let mut doc = doc!(
            self.f.title => d.title.clone(),
            self.f.body => d.body.clone(),
            self.f.path => d.path.clone(),
            self.f.file_id => d.file_id as u64,
            self.f.mtime => d.mtime_ms,
        );
        let mut prefixes = std::collections::HashSet::new();
        for w in fold(&d.title).split(' ') {
            let chars: Vec<char> = w.chars().collect();
            for k in 1..=chars.len().min(15) {
                prefixes.insert(chars[..k].iter().collect::<String>());
            }
        }
        for p in prefixes {
            doc.add_text(self.f.title_prefix, p);
        }
        for a in &d.aliases {
            doc.add_text(self.f.aliases, a);
        }
        for h in &d.headings {
            doc.add_text(self.f.headings, h);
        }
        for t in &d.tags {
            doc.add_text(self.f.tags, t);
            // Nested tags: "a/b" also matches tag:a
            let mut acc = String::new();
            for part in t.split('/') {
                if !acc.is_empty() {
                    acc.push('/');
                }
                acc.push_str(part);
                if acc != *t {
                    doc.add_text(self.f.tags, &acc);
                }
            }
            doc.add_text(self.f.tags_text, t.replace('/', " "));
        }
        let lower = d.path.to_lowercase();
        let mut acc = String::new();
        for part in lower.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            doc.add_text(self.f.path_prefix, &acc);
        }
        w.add_document(doc)?;
        Ok(())
    }

    pub fn delete(&self, file_id: u32) {
        let g = self.writer.0.lock().unwrap();
        let Some(w) = g.as_ref() else { return };
        w.delete_term(Term::from_field_u64(self.f.file_id, file_id as u64));
    }

    pub fn clear(&self) -> Result<()> {
        if let Some(w) = self.writer.0.lock().unwrap().as_mut() {
            w.delete_all_documents()?;
        }
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        if let Some(w) = self.writer.0.lock().unwrap().as_mut() {
            w.commit()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    /// Commit and release the writer (and its lock file). Further writes are ignored.
    pub fn close(&self) -> Result<()> {
        if let Some(mut w) = self.writer.0.lock().unwrap().take() {
            w.commit()?;
            w.wait_merging_threads()?;
        }
        self.reader.reload()?;
        Ok(())
    }

    /// True if the error means another process holds the index writer lock.
    pub fn is_lock_busy(e: &crate::SearchError) -> bool {
        matches!(e, crate::SearchError::Tantivy(tantivy::TantivyError::LockFailure(..)))
    }

    fn term_query(&self, field: Field, word: &str, boost: f32, fuzzy: bool) -> Box<dyn Query> {
        let term = Term::from_field_text(field, word);
        let n = word.chars().count();
        let dist = if !fuzzy || n < 4 { 0 } else if n < 8 { 1 } else { 2 };
        let q: Box<dyn Query> = if dist == 0 {
            Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
        } else {
            // Exact match scores above fuzzy neighbours.
            Box::new(BooleanQuery::new(vec![
                (Occur::Should, Box::new(BoostQuery::new(Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs)), 2.0)) as Box<dyn Query>),
                (Occur::Should, Box::new(FuzzyTermQuery::new(term, dist, true))),
            ]))
        };
        Box::new(BoostQuery::new(q, boost))
    }

    fn word_query(&self, word: &str, prefix: bool) -> Box<dyn Query> {
        let w = fold(word);
        let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for w in w.split(' ').filter(|s| !s.is_empty()) {
            let mut fields: Vec<(Occur, Box<dyn Query>)> = vec![
                (Occur::Should, self.term_query(self.f.title, w, 4.0, true)),
                (Occur::Should, self.term_query(self.f.aliases, w, 3.5, true)),
                (Occur::Should, self.term_query(self.f.headings, w, 2.0, true)),
                (Occur::Should, self.term_query(self.f.tags_text, w, 2.0, false)),
                (Occur::Should, self.term_query(self.f.body, w, 1.0, true)),
            ];
            if prefix {
                fields.push((Occur::Should, self.term_query(self.f.title_prefix, w, 3.0, false)));
            }
            parts.push((Occur::Must, Box::new(BooleanQuery::new(fields))));
        }
        Box::new(BooleanQuery::new(parts))
    }

    fn clause_query(&self, c: &Clause, prefix: bool) -> Option<Box<dyn Query>> {
        Some(match c {
            Clause::Term(t) => self.word_query(t, prefix),
            Clause::Phrase(words) => {
                let terms: Vec<Term> = words.iter().map(|w| Term::from_field_text(self.f.body, &fold(w))).collect();
                let title_terms: Vec<Term> = words.iter().map(|w| Term::from_field_text(self.f.title, &fold(w))).collect();
                Box::new(BooleanQuery::new(vec![
                    (Occur::Should, Box::new(PhraseQuery::new(terms)) as Box<dyn Query>),
                    (Occur::Should, Box::new(BoostQuery::new(Box::new(PhraseQuery::new(title_terms)), 4.0))),
                ]))
            }
            Clause::Tag(t) => Box::new(TermQuery::new(Term::from_field_text(self.f.tags, t), IndexRecordOption::Basic)),
            Clause::Path(p) => Box::new(TermQuery::new(Term::from_field_text(self.f.path_prefix, p), IndexRecordOption::Basic)),
            Clause::Title(t) => {
                let parts: Vec<(Occur, Box<dyn Query>)> =
                    fold(t).split(' ').filter(|s| !s.is_empty()).map(|w| (Occur::Must, self.term_query(self.f.title, w, 4.0, true))).collect();
                if parts.is_empty() {
                    return None;
                }
                Box::new(BooleanQuery::new(parts))
            }
        })
    }

    /// Run a query in the Section 6.1 syntax. Falls back to OR semantics if AND finds nothing.
    pub fn search(&self, q: &str, limit: usize, snippets: bool) -> Result<Vec<TextHit>> {
        let pq = parse_query(q);
        if pq.must.is_empty() {
            return Ok(Vec::new());
        }
        let n = pq.must.len();
        let build = |occur: Occur| -> BooleanQuery {
            let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (i, c) in pq.must.iter().enumerate() {
                let is_last = i + 1 == n && pq.trailing_word;
                let filter = matches!(c, Clause::Tag(_) | Clause::Path(_));
                if let Some(qq) = self.clause_query(c, is_last) {
                    parts.push((if filter { Occur::Must } else { occur }, qq));
                }
            }
            for c in &pq.exclude {
                if let Some(qq) = self.clause_query(c, false) {
                    parts.push((Occur::MustNot, qq));
                }
            }
            BooleanQuery::new(parts)
        };
        let searcher = self.reader.searcher();
        let mut query = build(Occur::Must);
        let mut top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        if top.is_empty() && pq.must.iter().filter(|c| !matches!(c, Clause::Tag(_) | Clause::Path(_))).count() > 1 {
            query = build(Occur::Should);
            top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;
        }
        let snip_gen = if snippets {
            let mut g = SnippetGenerator::create(&searcher, &query, self.f.body)?;
            g.set_max_num_chars(160);
            Some(g)
        } else {
            None
        };
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            let file_id = d.get_first(self.f.file_id).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let snippet = match &snip_gen {
                Some(g) => g.snippet_from_doc(&d).to_html(),
                None => String::new(),
            };
            out.push(TextHit { file_id, score, snippet });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> TextIndex {
        let t = TextIndex::in_memory().unwrap();
        let docs = [
            (1, "Projects/Rust ownership.md", "Rust ownership", vec!["lang/rust"], "Borrowing and lifetimes keep memory safe without a garbage collector."),
            (2, "Cooking/Ramen.md", "Ramen", vec!["food"], "Tonkotsu broth needs twelve hours. Zero copy of the recipe."),
            (3, "Projects/Search engine.md", "Search engine", vec!["lang/rust", "draft"], "Tantivy gives BM25 and zero copy mmap segments in rust."),
        ];
        for (id, path, title, tags, body) in docs {
            t.upsert(&IndexDoc { file_id: id, path: path.into(), title: title.into(), tags: tags.into_iter().map(String::from).collect(), body: body.into(), ..Default::default() }).unwrap();
        }
        t.commit().unwrap();
        t
    }

    fn ids(h: &[TextHit]) -> Vec<u32> {
        h.iter().map(|x| x.file_id).collect()
    }

    #[test]
    fn syntax_and_boosts() {
        let t = idx();
        assert_eq!(ids(&t.search("rust", 10, false).unwrap())[0], 1, "title boost");
        assert_eq!(ids(&t.search("\"zero copy\" tag:lang", 10, false).unwrap()), vec![3]);
        assert_eq!(ids(&t.search("rust -draft", 10, false).unwrap()), vec![1]);
        assert_eq!(ids(&t.search("path:cooking", 10, false).unwrap()), vec![2], "filter-only query lists matches");
        assert_eq!(ids(&t.search("broth path:Cooking", 10, false).unwrap()), vec![2]);
        assert_eq!(ids(&t.search("garbage colector", 10, false).unwrap()), vec![1], "fuzzy");
        assert_eq!(ids(&t.search("own", 10, false).unwrap()), vec![1], "title prefix as-you-type");
        assert_eq!(ids(&t.search("title:ramen", 10, false).unwrap()), vec![2]);
        let h = t.search("tonkotsu", 10, true).unwrap();
        assert!(h[0].snippet.contains("<b>Tonkotsu</b>"), "{}", h[0].snippet);
        t.delete(2);
        t.commit().unwrap();
        assert!(t.search("tonkotsu", 10, false).unwrap().is_empty());
    }
}
