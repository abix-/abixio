use std::collections::HashMap;

pub(crate) fn parse_query(query: &str) -> HashMap<String, String> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_query;

    #[test]
    fn parse_query_decodes_percent_escaped_values() {
        let params = parse_query("prefix=docs%2F&delimiter=%2F&bucket=my%20bucket");

        assert_eq!(params.get("prefix").map(String::as_str), Some("docs/"));
        assert_eq!(params.get("delimiter").map(String::as_str), Some("/"));
        assert_eq!(params.get("bucket").map(String::as_str), Some("my bucket"));
    }
}
