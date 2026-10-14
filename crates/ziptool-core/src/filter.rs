//! 與 7-Zip 的 `-i`／`-x` 相同的名稱篩選：萬用字元 `*`、`?`，不分大小寫。
//!
//! 樣式不含 `/` 時比對名稱；含 `/` 時比對路徑。資料夾符合時連同其中的內容。
//! 非遞迴（`-x!`）只比對最上層，遞迴（`-xr!`）比對每一層。

/// 一個篩選樣式
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    parts: Vec<String>,
    recursive: bool,
}

impl Pattern {
    pub fn new(text: &str, recursive: bool) -> Self {
        let text = crate::util::normalize_name(text).to_lowercase();
        // 與 7-Zip 相同：*.* 代表所有名稱（包含沒有副檔名的）
        let parts = text.split('/').map(|p| if p == "*.*" { "*".to_string() } else { p.to_string() }).collect();
        Self { parts, recursive }
    }

    /// 名稱（或它所在的資料夾）符合樣式
    fn hits(&self, name: &str) -> bool {
        let name = crate::util::normalize_name(name).to_lowercase();
        let parts: Vec<&str> = name.split('/').collect();
        let k = self.parts.len();
        if parts.len() < k {
            return false;
        }
        let last = if self.recursive { parts.len() - k } else { 0 };
        (0..=last).any(|start| self.parts.iter().zip(&parts[start..]).all(|(p, n)| wildcard(p, n)))
    }
}

/// 加入與解壓縮時的篩選條件；沒有任何樣式時全部通過
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    /// 只處理符合的項目（`-i`）；空的代表全部
    pub include: Vec<Pattern>,
    /// 略過符合的項目（`-x`），優先於 `include`
    pub exclude: Vec<Pattern>,
}

impl Filter {
    pub fn allows(&self, name: &str) -> bool {
        !self.excludes(name) && (self.include.is_empty() || self.include.iter().any(|p| p.hits(name)))
    }

    /// 被排除：資料夾被排除時不必再往下找
    pub(crate) fn excludes(&self, name: &str) -> bool {
        self.exclude.iter().any(|p| p.hits(name))
    }
}

/// `*` 代表任意個字元，`?` 代表一個字元
fn wildcard(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut i, mut j) = (0, 0);
    // 上一個 * 的位置與當時比對到的字元，用來回溯
    let mut star: Option<(usize, usize)> = None;
    while j < s.len() {
        if i < p.len() && (p[i] == '?' || p[i] == s[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards() {
        assert!(wildcard("*.txt", "a.txt"));
        assert!(wildcard("*", ""));
        assert!(wildcard("a?c", "abc"));
        assert!(wildcard("*報告*", "期末報告.docx"));
        assert!(!wildcard("*.txt", "a.txt.bak"));
        assert!(!wildcard("a?c", "ac"));
        assert!(wildcard("*a*b", "xaxxab"));
    }

    #[test]
    fn top_level_or_every_level() {
        let top = Filter { exclude: vec![Pattern::new("*.log", false)], ..Default::default() };
        assert!(!top.allows("a.log"));
        assert!(top.allows("sub/a.log"), "非遞迴只比對最上層");
        let all = Filter { exclude: vec![Pattern::new("*.LOG", true)], ..Default::default() };
        assert!(!all.allows("sub/deep/A.log"), "遞迴且不分大小寫");
        assert!(all.allows("sub/a.txt"));
        // 資料夾符合時連同其中的內容
        let dir = Filter { exclude: vec![Pattern::new("node_modules", true)], ..Default::default() };
        assert!(!dir.allows("app/node_modules/x/index.js"));
        assert!(dir.allows("app/src/index.js"));
        // 含路徑的樣式
        let path = Filter { include: vec![Pattern::new("docs\\*.md", false)], ..Default::default() };
        assert!(path.allows("docs/a.md") && path.allows("docs/a.md/inner"));
        assert!(!path.allows("src/docs/a.md") && !path.allows("readme.md"));
        // 排除優先於包含；*.* 也符合沒有副檔名的名稱
        let both = Filter { include: vec![Pattern::new("*.*", true)], exclude: vec![Pattern::new("secret*", true)] };
        assert!(both.allows("Makefile"));
        assert!(!both.allows("a/secret.txt"));
    }
}
