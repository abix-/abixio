//! Tiny glob matcher supporting `*` (zero-or-more of any char) and
//! `?` (exactly one char). No character classes, no escaping. Used
//! for policy action names (`s3:Get*`) and resource ARNs
//! (`arn:aws:s3:::bucket/photos/*`).

pub fn glob_match(pattern: &str, s: &str) -> bool {
    let p = pattern.as_bytes();
    let t = s.as_bytes();
    match_inner(p, 0, t, 0)
}

fn match_inner(p: &[u8], mut pi: usize, t: &[u8], mut ti: usize) -> bool {
    // iterative NFA walk; track last `*` so we can backtrack the text
    // pointer one step at a time on mismatch.
    let mut star_p: Option<usize> = None;
    let mut star_t: usize = 0;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
            continue;
        }
        if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
            continue;
        }
        if let Some(sp) = star_p {
            pi = sp + 1;
            star_t += 1;
            ti = star_t;
            continue;
        }
        return false;
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn exact_match() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "helloo"));
        assert!(!glob_match("hello", "hell"));
    }

    #[test]
    fn star_matches_zero_or_more() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*", "a"));
        assert!(glob_match("a*", "abc"));
        assert!(glob_match("*c", "abc"));
        assert!(glob_match("a*c", "abbc"));
        assert!(!glob_match("a*c", "abd"));
    }

    #[test]
    fn question_matches_one() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn action_globs() {
        assert!(glob_match("s3:*", "s3:GetObject"));
        assert!(glob_match("s3:Get*", "s3:GetObject"));
        assert!(!glob_match("s3:Get*", "s3:PutObject"));
        assert!(glob_match("s3:GetObject", "s3:GetObject"));
    }

    #[test]
    fn resource_globs() {
        assert!(glob_match("arn:aws:s3:::pub/*", "arn:aws:s3:::pub/photo.jpg"));
        assert!(glob_match("arn:aws:s3:::pub/*", "arn:aws:s3:::pub/deep/key.png"));
        assert!(!glob_match("arn:aws:s3:::pub/*", "arn:aws:s3:::other/k"));
        assert!(glob_match("arn:aws:s3:::pub", "arn:aws:s3:::pub"));
    }
}
