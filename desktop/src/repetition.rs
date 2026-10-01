//! 「重复语音」清洗：把逐字累积式的重复文本洗成最终完整的那一段。
//!
//! 典型输入（Moonlight / 语音识别的逐字回吐）：
//! `语语音语音输…像这样的重复输入的文字。语音输入，像这样的重复输入的文字。`
//! 期望输出：`语音输入，像这样的重复输入的文字。`
//!
//! 判定思路：找到文本末尾的那一段「最终文字」F，使得 F 之前的部分可以拆成
//! 若干段，每段都是 F 的前缀，且长度递增（越说越完整）。标点只参与保留，
//! 不参与匹配，避免「像这样的……。像这样的……」里的句号打断累积。

/// 这些字符视为「噪声」：空白与各种标点。检测重复时忽略它们，输出时保留。
fn is_noise(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '。' | '，'
                | '、'
                | '！'
                | '？'
                | '；'
                | '：'
                | ','
                | '.'
                | '!'
                | '?'
                | ';'
                | ':'
                | '~'
                | '～'
                | '-'
                | '—'
                | '…'
                | '"'
                | '\''
                | '“'
                | '”'
                | '‘'
                | '’'
                | '（'
                | '）'
                | '('
                | ')'
                | '《'
                | '》'
                | '〈'
                | '〉'
                | '【'
                | '】'
                | '['
                | ']'
        )
}

fn strip(text: &str) -> Vec<char> {
    text.chars().filter(|ch| !is_noise(*ch)).collect()
}

/// core[pos..limit] 与 f 从头开始能匹配多少个字符。
fn prefix_match(core: &[char], pos: usize, f: &[char], limit: usize) -> usize {
    let mut len = 0;
    while pos + len < limit && len < f.len() && core[pos + len] == f[len] {
        len += 1;
    }
    len
}

/// hay 中是否包含连续子串 f。
fn contains(hay: &[char], f: &[char]) -> bool {
    if f.is_empty() || f.len() > hay.len() {
        return false;
    }
    hay.windows(f.len()).any(|window| window == f)
}

/// 累积式：整段可拆成 >=3 段，每段都是末尾 F 的前缀，长度严格递增，
/// 且 F 比最后一段更长（说明 F 是最终完整句）。
fn chain_clean(core: &[char]) -> Option<Vec<char>> {
    let n = core.len();
    let mut k = n;
    while k > 1 {
        k -= 1;
        let f = &core[k..];
        if f.len() < 2 {
            continue;
        }

        let mut pos = 0usize;
        let mut prev = 0usize;
        let mut chunks = 0usize;
        let mut ok = true;
        while pos < k {
            let len = prefix_match(core, pos, f, k);
            if len == 0 || len <= prev {
                ok = false;
                break;
            }
            pos += len;
            prev = len;
            chunks += 1;
        }

        if ok && pos == k && chunks >= 3 && prev < f.len() {
            return Some(f.to_vec());
        }
    }
    None
}

/// 重复式：末尾 F 之前已有较长的一段内容（至少 F 的 ratio 倍），
/// 说明前面是冗余的反复，F 才是最终要留下的。
fn suffix_repeat(core: &[char], min_f: usize, ratio: usize) -> Option<Vec<char>> {
    let n = core.len();
    let mut k = 1;
    while k + min_f <= n {
        let f = &core[k..];
        if k >= f.len() * ratio && contains(&core[..k], f) {
            return Some(f.to_vec());
        }
        k += 1;
    }
    None
}

/// 从原文里取出「最后 count 个非噪声字符」所在的那一段，保留段内标点。
fn extract_suffix(text: &str, count: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut seen = 0usize;
    let mut start = chars.len();
    while start > 0 && seen < count {
        start -= 1;
        if !is_noise(chars[start]) {
            seen += 1;
        }
    }

    let mut out: String = chars[start..].iter().collect();
    let leading = out.chars().take_while(|ch| is_noise(*ch)).count();
    if leading > 0 {
        out = out.chars().skip(leading).collect();
    }
    out
}

fn clean_once(text: &str) -> Option<String> {
    let core = strip(text);
    if core.is_empty() {
        return None;
    }
    let hit = suffix_repeat(&core, 4, 5).or_else(|| chain_clean(&core))?;
    if hit == core {
        return None;
    }
    Some(extract_suffix(text, hit.len()))
}

/// 反复清洗直到稳定；无可清洗内容时返回 None。
pub fn clean_repetition(text: &str) -> Option<String> {
    let mut current = text.to_string();
    let mut changed = false;
    for _ in 0..8 {
        match clean_once(&current) {
            Some(next) if next != current => {
                current = next;
                changed = true;
            }
            _ => break,
        }
    }
    if changed {
        Some(current)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::clean_repetition;

    const LONG: &str = "语语音语音输语音输入语音输入。语音输入。像语音输入。像这语音输入。像这样的语音输入。像这样的重语音输入。像这样的重复语音输入。像这样的重复输语音输入。像这样的重复输入语音输入。像这样的重复输入的语音输入。像这样的重复输入的文语音输入。像这样的重复输入的文字语音输入。像这样的重复输入的文字。语音输入，像这样的重复输入的文字。";

    #[test]
    fn cleans_accumulating_example() {
        assert_eq!(
            clean_repetition(LONG).as_deref(),
            Some("语音输入，像这样的重复输入的文字。")
        );
    }

    #[test]
    fn cleans_growing_words() {
        assert_eq!(
            clean_repetition("我我们我们明天我们明天去我们明天去公园").as_deref(),
            Some("我们明天去公园")
        );
    }

    #[test]
    fn leaves_normal_text_alone() {
        let corpus = [
            "这是一段正常的话没有任何重复请不要改动",
            "今天天气不错，我们下午三点开会，记得带上笔记本。",
            "明天记得把文件发给我，谢谢。",
            "这个项目的进度怎么样了，需要我帮忙吗？",
            "我想问问你周末有什么安排",
            "好的，谢谢",
            "你好，请问在吗",
            "打开设置然后找到网络选项",
            "今天的会议纪要：第一点，第二点，第三点。",
            "今天下午三点开会记得带上电脑和充电器",
            "哈哈哈哈",
            "我们我们",
            "abcabcabc",
            "好的好的",
            "谢谢谢谢",
        ];
        for text in corpus {
            assert_eq!(clean_repetition(text), None, "不应改动：{text}");
        }
    }
}
