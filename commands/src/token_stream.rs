/// Parsed tokens with methods for consumption
#[derive(Clone, Debug)]
pub struct TokenStream {
    /// Original input string
    input: String,

    /// Parsed tokens
    tokens: Vec<String>,
}

impl TokenStream {
    /// Create TokenStream with simple whitespace tokenization (for completion)
    pub fn new(input: String) -> Self {
        let tokens = input.split_whitespace().map(String::from).collect();
        Self { input, tokens }
    }

    /// Create TokenStream respecting quotes (for command argument parsing)
    pub fn from_quoted(input: impl Into<String>) -> Self {
        let input = input.into();
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            match ch {
                '"' => {
                    in_quotes = !in_quotes;
                }
                ' ' | '\t' if !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(current.clone());
                        current.clear();
                    }
                }
                '\\' if in_quotes => {
                    // Handle escape sequences in quotes
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                _ => {
                    current.push(ch);
                }
            }
        }

        if !current.is_empty() {
            tokens.push(current);
        }

        Self { input, tokens }
    }

    /// Get token at index
    pub fn get(&self, index: usize) -> Option<&str> {
        self.tokens.get(index).map(|s| s.as_str())
    }

    /// Get the last token (what's being completed)
    pub fn last(&self) -> Option<&str> {
        self.tokens.last().map(|s| s.as_str())
    }

    /// Number of tokens
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Is empty
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Parse token at index as type T (returns Option for simple cases)
    pub fn parse<T: std::str::FromStr>(&self, index: usize) -> Option<T> {
        self.get(index).and_then(|s| s.parse().ok())
    }

    /// Parse token at index with error handling (for command execution)
    pub fn parse_arg<T>(&self, index: usize) -> Result<T, crate::error::ParseError>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display + std::fmt::Debug + Send + Sync + 'static,
    {
        let value = self.get(index)
            .ok_or(crate::error::ParseError::MissingArg(index))?;

        value.parse()
            .map_err(|e| crate::error::ParseError::Custom(
                format!("Failed to parse argument at position {}: {}", index, e)
            ))
    }

    /// Parse optional token at index (for Option<T> arguments)
    pub fn parse_optional<T>(&self, index: usize) -> Result<Option<T>, crate::error::ParseError>
    where
        T: std::str::FromStr,
        T::Err: std::fmt::Display + std::fmt::Debug + Send + Sync + 'static,
    {
        if self.get(index).is_some() {
            self.parse_arg(index).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Calculate which argument index is being completed
    pub fn arg_index(&self) -> usize {
        if self.input.ends_with(char::is_whitespace) {
            self.tokens.len().saturating_sub(1)
        } else {
            self.tokens.len().saturating_sub(2)
        }
    }

    /// Get the partial word being completed
    pub fn partial(&self) -> &str {
        if self.input.ends_with(char::is_whitespace) {
            ""  // Completing a new word
        } else {
            self.last().unwrap_or("")
        }
    }

    /// Get the original input string
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Get the cursor position
    pub fn cursor(&self) -> usize {
        self.input.len()
    }

    /// Parse command name from input (expects leading slash)
    /// Returns None if input doesn't start with '/' or has no command name
    pub fn command_name(&self) -> Option<&str> {
        let trimmed = self.input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }

        // Get first token and strip the leading slash
        self.get(0).and_then(|token| token.strip_prefix('/'))
    }

    /// Extract the arguments portion after the command name
    /// For "/command arg1 arg2", returns "arg1 arg2"
    pub fn args_string(&self) -> &str {
        let input = self.input.trim();

        // Find first whitespace after command name
        if let Some(space_pos) = input.find(char::is_whitespace) {
            input[space_pos..].trim_start()
        } else {
            // No arguments
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_simple_whitespace() {
        let ts = TokenStream::new("foo bar baz".to_string());
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.get(0), Some("foo"));
        assert_eq!(ts.get(1), Some("bar"));
        assert_eq!(ts.get(2), Some("baz"));
        assert_eq!(ts.last(), Some("baz"));
        assert_eq!(ts.input(), "foo bar baz");
        assert_eq!(ts.cursor(), 11);
    }

    #[test]
    fn test_new_multiple_spaces() {
        let ts = TokenStream::new("foo    bar".to_string());
        assert_eq!(ts.len(), 2);
        assert_eq!(ts.get(0), Some("foo"));
        assert_eq!(ts.get(1), Some("bar"));
    }

    #[test]
    fn test_new_empty() {
        let ts = TokenStream::new("".to_string());
        assert_eq!(ts.len(), 0);
        assert!(ts.is_empty());
        assert_eq!(ts.last(), None);
    }

    #[test]
    fn test_from_quoted_simple() {
        let ts = TokenStream::from_quoted("foo bar baz");
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.get(0), Some("foo"));
        assert_eq!(ts.get(1), Some("bar"));
        assert_eq!(ts.get(2), Some("baz"));
    }

    #[test]
    fn test_from_quoted_with_quotes() {
        let ts = TokenStream::from_quoted(r#"foo "hello world" bar"#);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.get(0), Some("foo"));
        assert_eq!(ts.get(1), Some("hello world"));
        assert_eq!(ts.get(2), Some("bar"));
    }

    #[test]
    fn test_from_quoted_escaped_quotes() {
        let ts = TokenStream::from_quoted(r#""hello \"world\"""#);
        assert_eq!(ts.len(), 1);
        assert_eq!(ts.get(0), Some(r#"hello "world""#));
    }

    #[test]
    fn test_from_quoted_multiple_quoted_strings() {
        let ts = TokenStream::from_quoted(r#""first string" "second string" normal"#);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.get(0), Some("first string"));
        assert_eq!(ts.get(1), Some("second string"));
        assert_eq!(ts.get(2), Some("normal"));
    }

    #[test]
    fn test_parse_type_conversion() {
        let ts = TokenStream::new("42 2.5 true".to_string());
        assert_eq!(ts.parse::<i32>(0), Some(42));
        assert_eq!(ts.parse::<f64>(1), Some(2.5));
        assert_eq!(ts.parse::<bool>(2), Some(true));
    }

    #[test]
    fn test_parse_invalid_type() {
        let ts = TokenStream::new("not_a_number".to_string());
        assert_eq!(ts.parse::<i32>(0), None);
    }

    #[test]
    fn test_parse_out_of_bounds() {
        let ts = TokenStream::new("foo".to_string());
        assert_eq!(ts.parse::<String>(5), None);
    }

    #[test]
    fn test_arg_index_with_trailing_space() {
        // "/model gemini " - cursor after space, completing new arg
        let ts = TokenStream::new("/model gemini ".to_string());
        assert_eq!(ts.len(), 2); // ["model", "gemini"]
        assert_eq!(ts.arg_index(), 1); // tokens.len() - 1 = 2 - 1 = 1
    }

    #[test]
    fn test_arg_index_without_trailing_space() {
        // "/model gem" - cursor in middle of word, completing current arg
        let ts = TokenStream::new("/model gem".to_string());
        assert_eq!(ts.len(), 2); // ["model", "gem"]
        assert_eq!(ts.arg_index(), 0); // tokens.len() - 2 = 2 - 2 = 0
    }

    #[test]
    fn test_arg_index_single_token() {
        // "/mod" - completing command name
        let ts = TokenStream::new("/mod".to_string());
        assert_eq!(ts.len(), 1); // ["mod"]
        assert_eq!(ts.arg_index(), 0); // saturating_sub prevents underflow
    }

    #[test]
    fn test_partial_with_trailing_space() {
        // "/model gemini " - completing new word
        let ts = TokenStream::new("/model gemini ".to_string());
        assert_eq!(ts.partial(), "");
    }

    #[test]
    fn test_partial_without_trailing_space() {
        // "/model gem" - completing "gem"
        let ts = TokenStream::new("/model gem".to_string());
        assert_eq!(ts.partial(), "gem");
    }

    #[test]
    fn test_partial_single_token() {
        // "/mod" - completing "/mod" (slash is included in token)
        let ts = TokenStream::new("/mod".to_string());
        assert_eq!(ts.partial(), "/mod");
    }

    #[test]
    fn test_partial_empty_input() {
        let ts = TokenStream::new("".to_string());
        assert_eq!(ts.partial(), "");
    }

    #[test]
    fn test_arg_index_and_partial_together() {
        // Test the common case: "/command arg1 arg2 par"
        let ts = TokenStream::new("/command arg1 arg2 par".to_string());
        assert_eq!(ts.len(), 4); // ["command", "arg1", "arg2", "par"]
        assert_eq!(ts.arg_index(), 2); // completing arg index 2 (3rd arg)
        assert_eq!(ts.partial(), "par");
    }

    #[test]
    fn test_arg_index_with_space_after_complete_word() {
        // "/command arg1 arg2 " - ready for next arg
        let ts = TokenStream::new("/command arg1 arg2 ".to_string());
        assert_eq!(ts.len(), 3); // ["command", "arg1", "arg2"]
        assert_eq!(ts.arg_index(), 2); // completing arg index 2 (3rd arg)
        assert_eq!(ts.partial(), ""); // empty partial for new word
    }

    #[test]
    fn test_command_name() {
        let ts = TokenStream::new("/help".to_string());
        assert_eq!(ts.command_name(), Some("help"));

        let ts2 = TokenStream::new("/model gemini".to_string());
        assert_eq!(ts2.command_name(), Some("model"));

        let ts3 = TokenStream::new("not a command".to_string());
        assert_eq!(ts3.command_name(), None);

        let ts4 = TokenStream::new("".to_string());
        assert_eq!(ts4.command_name(), None);

        let ts5 = TokenStream::new("/".to_string());
        assert_eq!(ts5.command_name(), Some(""));
    }

    #[test]
    fn test_args_string() {
        let ts = TokenStream::new("/help".to_string());
        assert_eq!(ts.args_string(), "");

        let ts2 = TokenStream::new("/set provider1".to_string());
        assert_eq!(ts2.args_string(), "provider1");

        let ts3 = TokenStream::new("/configure provider1 model1".to_string());
        assert_eq!(ts3.args_string(), "provider1 model1");

        let ts4 = TokenStream::new("/cmd   arg1   arg2".to_string());
        assert_eq!(ts4.args_string(), "arg1   arg2");
    }
}
