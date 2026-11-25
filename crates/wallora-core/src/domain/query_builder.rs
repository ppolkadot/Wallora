//! Typed builder and parser for the documented Wallhaven search query syntax.
//!
//! Documented tokens (`https://wallhaven.cc/help/api`): `tagname`, `-tagname`, `+tagname`,
//! `@username`, `id:N`, `type:png` / `type:jpg`, and `like:ID`. Phrases containing whitespace are
//! written in double quotes (`"red hair"`, `-"red hair"`).
//!
//! Two guarantees:
//! - Structured tokens render deterministically and parse back to the same tokens.
//! - Text that is not edited through tokens passes through [`SearchTerms::Raw`] byte-for-byte, so
//!   the app never rewrites what the user typed.

use std::fmt;

use thiserror::Error;

use crate::domain::WallpaperId;

/// File types accepted by `type:`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileType {
    Png,
    Jpg,
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpg => "jpg",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        if raw.eq_ignore_ascii_case("png") {
            Some(Self::Png)
        } else if raw.eq_ignore_ascii_case("jpg") {
            Some(Self::Jpg)
        } else {
            None
        }
    }
}

/// One search token. [`QueryToken::Raw`] carries any segment the parser does not recognise,
/// verbatim, so unknown syntax is never lost.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QueryToken {
    /// `tagname` or `"two words"`.
    Tag(String),
    /// `+tagname`.
    RequireTag(String),
    /// `-tagname`.
    ExcludeTag(String),
    /// `@username`.
    User(String),
    /// `id:N` exact tag search.
    TagId(u64),
    /// `type:png` or `type:jpg`.
    FileType(FileType),
    /// `like:ID` similar-wallpaper search.
    Like(WallpaperId),
    /// Unrecognised text, emitted exactly as given.
    Raw(String),
}

/// Why a structured token could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum QueryTokenError {
    #[error("query token values must not be empty")]
    Empty,
    #[error("this value cannot be written as a single Wallhaven query token")]
    Unrepresentable,
}

fn quote_if_needed(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

impl QueryToken {
    /// Renders the token in Wallhaven query syntax.
    pub fn render(&self) -> String {
        match self {
            Self::Tag(value) => quote_if_needed(value),
            Self::RequireTag(value) => format!("+{}", quote_if_needed(value)),
            Self::ExcludeTag(value) => format!("-{}", quote_if_needed(value)),
            Self::User(name) => format!("@{name}"),
            Self::TagId(id) => format!("id:{id}"),
            Self::FileType(kind) => format!("type:{}", kind.as_str()),
            Self::Like(id) => format!("like:{id}"),
            Self::Raw(text) => text.clone(),
        }
    }

    /// Accepts a structured token only if it survives a render/parse round trip unchanged, which
    /// rejects values that would be re-read as a different token.
    fn validated(self, value: &str) -> Result<Self, QueryTokenError> {
        if value.trim().is_empty() {
            return Err(QueryTokenError::Empty);
        }
        if parse_tokens(&self.render()) == std::slice::from_ref(&self) {
            Ok(self)
        } else {
            Err(QueryTokenError::Unrepresentable)
        }
    }

    pub fn tag(value: impl Into<String>) -> Result<Self, QueryTokenError> {
        let value = value.into();
        Self::Tag(value.clone()).validated(&value)
    }

    pub fn require_tag(value: impl Into<String>) -> Result<Self, QueryTokenError> {
        let value = value.into();
        Self::RequireTag(value.clone()).validated(&value)
    }

    pub fn exclude_tag(value: impl Into<String>) -> Result<Self, QueryTokenError> {
        let value = value.into();
        Self::ExcludeTag(value.clone()).validated(&value)
    }

    pub fn user(name: impl Into<String>) -> Result<Self, QueryTokenError> {
        let name = name.into();
        Self::User(name.clone()).validated(&name)
    }

    pub fn tag_id(id: u64) -> Self {
        Self::TagId(id)
    }

    pub fn file_type(kind: FileType) -> Self {
        Self::FileType(kind)
    }

    pub fn like(id: impl Into<String>) -> Result<Self, QueryTokenError> {
        let id = id.into();
        Self::Like(WallpaperId::new(id.clone())).validated(&id)
    }
}

impl fmt::Display for QueryToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// Splits `raw` into tokens. Never fails: unrecognised segments become [`QueryToken::Raw`].
pub fn parse_tokens(raw: &str) -> Vec<QueryToken> {
    let chars: Vec<(usize, char)> = raw.char_indices().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1.is_whitespace() {
            i += 1;
            continue;
        }
        let (token, next) = parse_segment(raw, &chars, i);
        tokens.push(token);
        i = next;
    }
    tokens
}

fn byte_at(raw: &str, chars: &[(usize, char)], index: usize) -> usize {
    chars.get(index).map_or(raw.len(), |(byte, _)| *byte)
}

/// Index of the first whitespace at or after `from` (or the end).
fn word_end(chars: &[(usize, char)], from: usize) -> usize {
    (from..chars.len())
        .find(|&j| chars[j].1.is_whitespace())
        .unwrap_or(chars.len())
}

fn parse_segment(raw: &str, chars: &[(usize, char)], start: usize) -> (QueryToken, usize) {
    let prefix = match chars[start].1 {
        '+' => Some('+'),
        '-' => Some('-'),
        _ => None,
    };
    let body = start + usize::from(prefix.is_some());

    if chars.get(body).is_some_and(|(_, c)| *c == '"') {
        let Some(close) = (body + 1..chars.len()).find(|&j| chars[j].1 == '"') else {
            // Unterminated quote: everything to the end is one raw segment.
            let text = &raw[byte_at(raw, chars, start)..];
            return (QueryToken::Raw(text.to_string()), chars.len());
        };
        let after = close + 1;
        if after < chars.len() && !chars[after].1.is_whitespace() {
            let end = word_end(chars, after);
            let text = &raw[byte_at(raw, chars, start)..byte_at(raw, chars, end)];
            return (QueryToken::Raw(text.to_string()), end);
        }
        let phrase = &raw[byte_at(raw, chars, body + 1)..byte_at(raw, chars, close)];
        let segment = &raw[byte_at(raw, chars, start)..byte_at(raw, chars, after)];
        let token = if phrase.trim().is_empty() {
            QueryToken::Raw(segment.to_string())
        } else if phrase.chars().any(char::is_whitespace) {
            match prefix {
                Some('+') => QueryToken::RequireTag(phrase.to_string()),
                Some(_) => QueryToken::ExcludeTag(phrase.to_string()),
                None => QueryToken::Tag(phrase.to_string()),
            }
        } else {
            // A quoted single word renders without quotes, so it is only a tag if the bare word
            // would be read back as the same tag; otherwise (`"@x"`, `"id:5"`) keep it verbatim.
            match classify_word(prefix, phrase, segment) {
                tag @ (QueryToken::Tag(_)
                | QueryToken::RequireTag(_)
                | QueryToken::ExcludeTag(_)) => tag,
                _ => QueryToken::Raw(segment.to_string()),
            }
        };
        return (token, after);
    }

    let end = word_end(chars, start);
    let whole = &raw[byte_at(raw, chars, start)..byte_at(raw, chars, end)];
    let word = &raw[byte_at(raw, chars, body)..byte_at(raw, chars, end)];
    (classify_word(prefix, word, whole), end)
}

fn is_plain_value(word: &str) -> bool {
    !word.is_empty() && !word.contains('"') && !word.starts_with(['+', '-', '@'])
}

fn strip_keyword<'a>(word: &'a str, keyword: &str) -> Option<&'a str> {
    let head = word.get(..keyword.len())?;
    head.eq_ignore_ascii_case(keyword)
        .then(|| &word[keyword.len()..])
}

fn classify_word(prefix: Option<char>, word: &str, whole: &str) -> QueryToken {
    let raw = || QueryToken::Raw(whole.to_string());
    match prefix {
        Some('+') if is_plain_value(word) => QueryToken::RequireTag(word.to_string()),
        Some('-') if is_plain_value(word) => QueryToken::ExcludeTag(word.to_string()),
        Some(_) => raw(),
        None => {
            if let Some(name) = word.strip_prefix('@') {
                if !name.is_empty() && !name.contains('"') {
                    QueryToken::User(name.to_string())
                } else {
                    raw()
                }
            } else if let Some(digits) = strip_keyword(word, "id:") {
                match digits
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| digits.parse::<u64>().ok())
                    .flatten()
                {
                    Some(id) if !digits.is_empty() => QueryToken::TagId(id),
                    _ => raw(),
                }
            } else if let Some(kind) = strip_keyword(word, "type:") {
                FileType::parse(kind).map_or_else(raw, QueryToken::FileType)
            } else if let Some(id) = strip_keyword(word, "like:") {
                if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    QueryToken::Like(WallpaperId::new(id))
                } else {
                    raw()
                }
            } else if is_plain_value(word) {
                QueryToken::Tag(word.to_string())
            } else {
                raw()
            }
        }
    }
}

/// Search text for the `q` parameter: either exactly what the user typed, or tokens we render.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SearchTerms {
    /// Passed to the API unchanged (no trimming, no normalisation).
    Raw(String),
    /// Structured tokens joined by single spaces.
    Tokens(Vec<QueryToken>),
}

impl Default for SearchTerms {
    fn default() -> Self {
        Self::Raw(String::new())
    }
}

impl SearchTerms {
    pub fn raw(text: impl Into<String>) -> Self {
        Self::Raw(text.into())
    }

    pub fn from_tokens(tokens: Vec<QueryToken>) -> Self {
        Self::Tokens(tokens)
    }

    /// The exact `q` value.
    pub fn render(&self) -> String {
        match self {
            Self::Raw(text) => text.clone(),
            Self::Tokens(tokens) => tokens
                .iter()
                .map(QueryToken::render)
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// The tokens this text means, parsing raw text on demand without altering it.
    pub fn tokens(&self) -> Vec<QueryToken> {
        match self {
            Self::Raw(text) => parse_tokens(text),
            Self::Tokens(tokens) => tokens.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Raw(text) => text.is_empty(),
            Self::Tokens(tokens) => tokens.is_empty(),
        }
    }

    /// `q` for [`crate::domain::SearchQuery`]: `None` when there is nothing to search for.
    pub fn to_param(&self) -> Option<String> {
        let rendered = self.render();
        (!rendered.is_empty()).then_some(rendered)
    }
}

/// Fluent builder for structured searches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryBuilder {
    tokens: Vec<QueryToken>,
}

impl QueryBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts from existing text, keeping every recognised and unrecognised segment.
    pub fn from_text(raw: &str) -> Self {
        Self {
            tokens: parse_tokens(raw),
        }
    }

    pub fn push(mut self, token: QueryToken) -> Self {
        self.tokens.push(token);
        self
    }

    pub fn tag(self, value: impl Into<String>) -> Result<Self, QueryTokenError> {
        Ok(self.push(QueryToken::tag(value)?))
    }

    pub fn require_tag(self, value: impl Into<String>) -> Result<Self, QueryTokenError> {
        Ok(self.push(QueryToken::require_tag(value)?))
    }

    pub fn exclude_tag(self, value: impl Into<String>) -> Result<Self, QueryTokenError> {
        Ok(self.push(QueryToken::exclude_tag(value)?))
    }

    pub fn user(self, name: impl Into<String>) -> Result<Self, QueryTokenError> {
        Ok(self.push(QueryToken::user(name)?))
    }

    pub fn tag_id(self, id: u64) -> Self {
        self.push(QueryToken::tag_id(id))
    }

    pub fn file_type(self, kind: FileType) -> Self {
        self.push(QueryToken::file_type(kind))
    }

    pub fn like(self, id: impl Into<String>) -> Result<Self, QueryTokenError> {
        Ok(self.push(QueryToken::like(id)?))
    }

    pub fn tokens(&self) -> &[QueryToken] {
        &self.tokens
    }

    pub fn render(&self) -> String {
        self.build().render()
    }

    pub fn build(&self) -> SearchTerms {
        SearchTerms::Tokens(self.tokens.clone())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn documented_tokens_parse_and_render() {
        let parsed = parse_tokens(r#"cat +night -"red hair" @bob id:12 type:PNG like:94x38z"#);
        assert_eq!(
            parsed,
            vec![
                QueryToken::Tag("cat".into()),
                QueryToken::RequireTag("night".into()),
                QueryToken::ExcludeTag("red hair".into()),
                QueryToken::User("bob".into()),
                QueryToken::TagId(12),
                QueryToken::FileType(FileType::Png),
                QueryToken::Like(WallpaperId::new("94x38z")),
            ]
        );
        assert_eq!(
            SearchTerms::from_tokens(parsed).render(),
            r#"cat +night -"red hair" @bob id:12 type:png like:94x38z"#
        );
    }

    #[test]
    fn malformed_segments_become_raw_and_stay_verbatim() {
        for text in [
            "id:abc",
            "id:",
            "type:gif",
            "like:",
            "like:ab-c",
            "@",
            "+",
            "-",
            "--x",
            "-@x",
            "\"unterminated phrase",
            "\"a\"b",
            "\"\"",
            "ab\"c",
            // Quoted single words that would change meaning once unquoted stay verbatim.
            "\"@\"",
            "\"@x\"",
            "\"id:5\"",
            "-\"-x\"",
        ] {
            assert_eq!(
                parse_tokens(text),
                vec![QueryToken::Raw(text.to_string())],
                "{text:?}"
            );
        }
    }

    #[test]
    fn constructors_reject_values_that_would_change_meaning() {
        assert_eq!(QueryToken::tag(""), Err(QueryTokenError::Empty));
        assert_eq!(QueryToken::tag("   "), Err(QueryTokenError::Empty));
        for ambiguous in ["-x", "+x", "@x", "id:5", "type:png", "like:abc", "a\"b"] {
            assert_eq!(
                QueryToken::tag(ambiguous),
                Err(QueryTokenError::Unrepresentable),
                "{ambiguous}"
            );
        }
        assert_eq!(
            QueryToken::user("two words"),
            Err(QueryTokenError::Unrepresentable)
        );
        assert_eq!(
            QueryToken::like("not valid"),
            Err(QueryTokenError::Unrepresentable)
        );
        assert_eq!(
            QueryToken::tag("red hair"),
            Ok(QueryToken::Tag("red hair".into()))
        );
        assert_eq!(
            QueryToken::tag("c:d"),
            Ok(QueryToken::Tag("c:d".into())),
            "colons alone are not keywords"
        );
    }

    #[test]
    fn builder_renders_deterministically() {
        let query = QueryBuilder::new()
            .tag("city")
            .unwrap()
            .exclude_tag("people")
            .unwrap()
            .user("alice")
            .unwrap()
            .file_type(FileType::Jpg)
            .tag_id(7);
        assert_eq!(query.render(), "city -people @alice type:jpg id:7");
        assert_eq!(query.render(), query.clone().render());
        assert_eq!(
            QueryBuilder::from_text(&query.render()).tokens(),
            query.tokens()
        );
    }

    #[test]
    fn raw_terms_pass_through_unchanged() {
        for text in [
            "",
            "  padded  ",
            "a\tb\n",
            "-\"x",
            "ünï cödé",
            "id:abc  +  -",
            "%20&q=x",
        ] {
            let terms = SearchTerms::raw(text);
            assert_eq!(terms.render(), text);
            assert_eq!(
                terms.to_param(),
                (!text.is_empty()).then(|| text.to_string())
            );
        }
    }

    fn word() -> impl Strategy<Value = String> {
        "[a-z0-9_]{1,12}"
    }

    fn phrase() -> impl Strategy<Value = String> {
        "[a-z]{1,6}( [a-z]{1,6}){1,2}"
    }

    fn value() -> impl Strategy<Value = String> {
        prop_oneof![word(), phrase()]
    }

    fn token() -> impl Strategy<Value = QueryToken> {
        prop_oneof![
            value().prop_map(QueryToken::Tag),
            value().prop_map(QueryToken::RequireTag),
            value().prop_map(QueryToken::ExcludeTag),
            "[A-Za-z0-9_]{1,20}".prop_map(QueryToken::User),
            (0u64..=u64::MAX).prop_map(QueryToken::TagId),
            prop_oneof![Just(FileType::Png), Just(FileType::Jpg)].prop_map(QueryToken::FileType),
            "[a-z0-9]{1,8}".prop_map(|id| QueryToken::Like(WallpaperId::new(id))),
        ]
    }

    proptest! {
        #[test]
        fn structured_tokens_round_trip(tokens in prop::collection::vec(token(), 0..8)) {
            let rendered = SearchTerms::from_tokens(tokens.clone()).render();
            prop_assert_eq!(parse_tokens(&rendered), tokens);
        }

        #[test]
        fn arbitrary_raw_text_is_never_rewritten(text in any::<String>()) {
            prop_assert_eq!(SearchTerms::raw(text.clone()).render(), text);
        }

        #[test]
        fn dense_syntax_text_parses_idempotently(text in "[a-z0-9\"+@: \\t-]{0,40}") {
            let first = parse_tokens(&text);
            let rendered = SearchTerms::from_tokens(first.clone()).render();
            prop_assert_eq!(parse_tokens(&rendered), first);
        }

        #[test]
        fn parsing_arbitrary_text_is_idempotent_and_lossless_for_raw_segments(text in "\\PC{0,60}") {
            let first = parse_tokens(&text);
            let rendered = SearchTerms::from_tokens(first.clone()).render();
            prop_assert_eq!(parse_tokens(&rendered), first.clone());
            // Rendering again is a fixed point.
            prop_assert_eq!(SearchTerms::from_tokens(parse_tokens(&rendered)).render(), rendered);
            // Raw segments are never altered.
            for token in &first {
                if let QueryToken::Raw(segment) = token {
                    prop_assert!(text.contains(segment.as_str()));
                }
            }
        }
    }
}
