//! File names: a template language, and suggestions drawn from the OCR text.
//!
//! Suggestions come from local heuristics (dates, document-type keywords and
//! the most prominent heading on page 1) or, when enabled, from a local LLM
//! server (Ollama or anything OpenAI-compatible). The LLM path always falls
//! back to the heuristics, so a stopped server never blocks a save.
//!
//! Template placeholders (case-insensitive):
//!
//! | Placeholder | Value |
//! |---|---|
//! | `{suggested}` | Suggested name ("Comcast Statement"), or "Scan" |
//! | `{title}` / `{type}` | The suggestion's parts (sender/heading, document type) |
//! | `{doc_date}` | Date found in the document, else today; takes a format like `{doc_date:%d.%m.%Y}` |
//! | `{date}` `{time}` `{datetime}` | Now (`2026-10-20`, `14-05-09`, `2026-10-20 14-05-09`); `{date:%B %Y}` etc. |
//! | `{counter}` / `{counter:4}` | Running number (zero-padded), stored in the config |
//! | `{pages}` `{profile}` `{scanner}` | Page count, profile name, device name |
//!
//! A `/` in the template makes subfolders (`{type}/{doc_date} {title}`).

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use jiff::civil::Date;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::{LlmApi, NamingSettings};
use crate::ocr::OcrPage;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    /// The ready-to-use name, e.g. "Comcast Statement".
    pub name: String,
    pub title: Option<String>,
    pub doc_type: Option<String>,
    pub doc_date: Option<Date>,
}

pub struct NameContext<'a> {
    pub now: jiff::Zoned,
    pub suggestion: Option<&'a Suggestion>,
    pub counter: u64,
    pub pages: usize,
    pub profile: &'a str,
    pub scanner: &'a str,
}

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{([A-Za-z_]+)(?::([^}]*))?\}").unwrap());

/// Expand a template into a relative path (one component per `/`), each
/// component sanitised for every OS's file system. Never empty.
pub fn render(template: &str, ctx: &NameContext) -> PathBuf {
    let s = ctx.suggestion;
    let expanded = PLACEHOLDER.replace_all(template, |c: &regex::Captures| {
        let fmt = c.get(2).map(|m| m.as_str());
        let strf = |f: &str, z: &jiff::Zoned| z.strftime(f).to_string();
        match c[1].to_ascii_lowercase().as_str() {
            "suggested" => s.map(|s| s.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Scan".into()),
            "title" => s.and_then(|s| s.title.clone()).unwrap_or_default(),
            "type" => s.and_then(|s| s.doc_type.clone()).unwrap_or_else(|| "Document".into()),
            "doc_date" => {
                let date = s.and_then(|s| s.doc_date).unwrap_or_else(|| ctx.now.date());
                date.strftime(fmt.unwrap_or("%Y-%m-%d")).to_string()
            }
            "date" => strf(fmt.unwrap_or("%Y-%m-%d"), &ctx.now),
            "time" => strf(fmt.unwrap_or("%H-%M-%S"), &ctx.now),
            "datetime" => strf(fmt.unwrap_or("%Y-%m-%d %H-%M-%S"), &ctx.now),
            "counter" => {
                let width = fmt.and_then(|f| f.parse::<usize>().ok()).unwrap_or(1);
                format!("{:0width$}", ctx.counter)
            }
            "pages" => ctx.pages.to_string(),
            "profile" => ctx.profile.to_string(),
            "scanner" => ctx.scanner.to_string(),
            _ => c[0].to_string(),
        }
    });
    let parts: Vec<String> = expanded.split(['/', '\\']).map(sanitize).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() { PathBuf::from("Scan") } else { parts.iter().collect() }
}

/// Make one path component safe on Windows, macOS and Linux.
pub fn sanitize(part: &str) -> String {
    let cleaned: String =
        part.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c }).collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.trim_matches(|c: char| c == '.' || c == ' ').chars().take(120).collect();
    // Windows reserves these names regardless of extension.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(&out)) {
        out.push('_');
    }
    out
}

/// Suggest a name for the document. Uses the LLM when configured and
/// reachable, the heuristics otherwise.
pub fn suggest(pages: &[&OcrPage], settings: &NamingSettings) -> Option<Suggestion> {
    let heuristic = suggest_heuristic(pages);
    if settings.use_llm {
        match suggest_llm(pages, settings) {
            Ok(mut s) => {
                // The model is good at titles, less so at dates; keep a date
                // the heuristics found when the model didn't give one.
                if s.doc_date.is_none() {
                    s.doc_date = heuristic.as_ref().and_then(|h| h.doc_date);
                }
                return Some(s);
            }
            Err(e) => log::warn!("LLM naming failed, using heuristics: {e:#}"),
        }
    }
    heuristic
}

// ─── Heuristics ────────────────────────────────────────────────────────────

const DOC_TYPES: &[(&str, &[&str])] = &[
    ("Invoice", &["invoice", "invoice number", "invoice no", "invoice #", "bill to", "remit to"]),
    (
        "Receipt",
        &[
            "receipt",
            "subtotal",
            "change due",
            "cashier",
            "thank you for",
            "transaction",
            "balance",
            "visa",
            "mastercard",
            "amex",
            "paid",
            "folio",
            "charges",
            "credits",
        ],
    ),
    ("Statement", &["statement", "account summary", "statement period", "beginning balance", "ending balance"]),
    ("Bill", &["amount due", "pay by", "total due", "past due"]),
    ("Pay Stub", &["pay stub", "pay period", "net pay", "gross pay", "year to date", "ytd"]),
    ("Tax Form", &["form w-2", "1099", "internal revenue service", "tax return", "irs", "taxpayer"]),
    // Not "signature"/"agree": receipts and folios carry those too.
    ("Contract", &["agreement", "hereinafter", "the parties", "terms and conditions", "witnesseth", "whereas"]),
    ("Insurance", &["policy number", "policyholder", "insured", "premium", "coverage", "claim number"]),
    (
        "Medical",
        &[
            "patient",
            "diagnosis",
            "prescription",
            "explanation of benefits",
            "physician",
            "guarantor",
            "office visit",
            "copay",
            "co-pay",
            "date of service",
            "provider",
            "clinic",
            "hospital",
            "pharmacy",
        ],
    ),
    ("Quote", &["quotation", "estimate", "quote number", "valid until"]),
    ("Order", &["order confirmation", "purchase order", "order number", "packing slip", "shipped"]),
    ("Certificate", &["certificate", "hereby certify", "certifies that"]),
    ("Letter", &["dear ", "sincerely", "kind regards", "best regards", "yours truly"]),
    ("Report", &["report", "summary of findings", "results"]),
    ("Form", &["please print", "fill out", "applicant", "check one"]),
];

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

static ISO_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b((?:19|20)\d\d)[-./](\d{1,2})[-./](\d{1,2})\b").unwrap());
static NUM_DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(\d{1,2})[-./](\d{1,2})[-./](\d{4}|\d{2})\b").unwrap());
static MONTH_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    // "March 4th, 2026", "Aug 19 2026", "Aug-19-2026"
    Regex::new(r"(?i)\b(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?[\s-]+(\d{1,2})(?:st|nd|rd|th)?,?[\s-]+((?:19|20)\d\d)\b").unwrap()
});
static DAY_FIRST: LazyLock<Regex> = LazyLock::new(|| {
    // "7 Feb 2025", "19-Aug-2026", "19. August 2026"
    Regex::new(r"(?i)\b(\d{1,2})(?:st|nd|rd|th)?\.?[\s-]+(jan|feb|mar|apr|may|jun|jul|aug|sep|sept|oct|nov|dec)[a-z]*\.?,?[\s-]+((?:19|20)\d\d)\b").unwrap()
});
static NOT_A_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(@|www\.|https?:|\.com\b|\b(street|st\.|avenue|ave\.?|road|rd\.|suite|ste\.|p\.?\s?o\.? box|blvd|drive|dr\.|lane|floor)\b|\bpage \d|\btel\b|\bphone\b|\bfax\b|\(\d{3}\)|\d{3}[-. ]\d{3}[-. ]\d{4})").unwrap()
});

fn month_index(name: &str) -> Option<i8> {
    let n = name.to_ascii_lowercase();
    MONTHS.iter().position(|m| n.starts_with(m)).map(|i| i as i8 + 1)
}

fn make_date(y: i32, m: i8, d: i8) -> Option<Date> {
    let y = if y < 100 { 2000 + y } else { y };
    let date = Date::new(y as i16, m, d).ok()?;
    let today = jiff::Zoned::now().date();
    // Plausibility: not before 1950, not more than a year ahead.
    (y >= 1950 && date <= today.saturating_add(jiff::Span::new().years(1))).then_some(date)
}

/// Every date in `text`, with its byte offset.
pub fn find_dates(text: &str) -> Vec<(usize, Date)> {
    let mut out = Vec::new();
    for c in ISO_DATE.captures_iter(text) {
        if let Some(d) = make_date(c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0), c[3].parse().unwrap_or(0)) {
            out.push((c.get(0).unwrap().start(), d));
        }
    }
    for c in NUM_DATE.captures_iter(text) {
        let (a, b): (i8, i8) = (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0));
        let y: i32 = c[3].parse().unwrap_or(0);
        // Month-first unless that's impossible (13/05/2026 is day-first).
        let d = make_date(y, a, b).or_else(|| make_date(y, b, a));
        if let Some(d) = d {
            out.push((c.get(0).unwrap().start(), d));
        }
    }
    for c in MONTH_FIRST.captures_iter(text) {
        if let (Some(m), Ok(d), Ok(y)) = (month_index(&c[1]), c[2].parse(), c[3].parse())
            && let Some(date) = make_date(y, m, d)
        {
            out.push((c.get(0).unwrap().start(), date));
        }
    }
    for c in DAY_FIRST.captures_iter(text) {
        if let (Some(m), Ok(d), Ok(y)) = (month_index(&c[2]), c[1].parse(), c[3].parse())
            && let Some(date) = make_date(y, m, d)
        {
            out.push((c.get(0).unwrap().start(), date));
        }
    }
    out.sort_by_key(|(pos, _)| *pos);
    out.dedup_by_key(|(pos, _)| *pos);
    out
}

/// The document's own date: the one labelled as such ("Invoice date:",
/// "Dated"), else the earliest-placed one, and never one labelled "due".
fn best_date(text: &str) -> Option<Date> {
    let lower = text.to_lowercase();
    find_dates(text)
        .into_iter()
        .map(|(pos, d)| {
            let before = &lower[pos.saturating_sub(40)..pos];
            let mut score = -(pos as f64) / 2000.0;
            if before.contains("date") || before.contains("dated") || before.contains("issued") {
                score += 3.0;
            }
            if before.contains("statement") || before.contains("invoice") {
                score += 1.0;
            }
            if before.contains("due") || before.contains("birth") || before.contains("dob") || before.contains("expir")
            {
                score -= 4.0;
            }
            (score, d)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, d)| d)
}

fn best_type(text: &str, heading: Option<&str>) -> Option<String> {
    let lower = text.to_lowercase();
    let heading = heading.map(str::to_lowercase).unwrap_or_default();
    let scores: Vec<(f64, &str)> = DOC_TYPES
        .iter()
        .map(|(name, words)| {
            let mut score: f64 = words.iter().map(|w| lower.matches(w).count().min(3) as f64).sum();
            if heading.contains(&name.to_lowercase()) {
                score += 5.0;
            }
            (score, *name)
        })
        .collect();
    let (_, best) = scores.iter().filter(|(s, _)| *s >= 2.0).max_by(|a, b| a.0.total_cmp(&b.0))?;
    // A payment document that is clearly medical says so — "Medical
    // Receipt" files and searches better than either word alone — whichever
    // of the two scored higher.
    let medical = scores.iter().any(|(s, n)| *n == "Medical" && *s >= 2.0);
    let payment = scores
        .iter()
        .filter(|(s, n)| *s >= 2.0 && matches!(*n, "Receipt" | "Bill" | "Statement" | "Invoice"))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    if let (true, Some((_, kind))) = (medical, payment)
        && matches!(*best, "Medical" | "Receipt" | "Bill" | "Statement" | "Invoice")
    {
        return Some(format!("Medical {kind}"));
    }
    Some(best.to_string())
}

fn tidy_case(s: &str) -> String {
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    let upper = letters.iter().filter(|c| c.is_uppercase()).count();
    if letters.len() > 3 && upper * 10 >= letters.len() * 8 {
        // ALL-CAPS headings → Title Case, keeping short all-caps tokens
        // (acronyms like "IRS", "LLC") as they are.
        s.split_whitespace()
            .map(|w| {
                if w.chars().filter(|c| c.is_alphabetic()).count() <= 3 {
                    w.to_string()
                } else {
                    let mut c = w.chars();
                    c.next()
                        .map(|f| f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect())
                        .unwrap_or_default()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        s.to_string()
    }
}

/// The most prominent heading-like line in the top part of page 1: big text
/// near the top, mostly letters, not an address, phone number or date.
fn best_heading(page: &OcrPage) -> Option<String> {
    let heights: Vec<f32> = page.lines.iter().map(|l| l.bbox.h).filter(|h| *h > 0.0).collect();
    if heights.is_empty() {
        return None;
    }
    let mut sorted = heights.clone();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2].max(1.0);
    let page_h = page.height.max(1) as f32;
    let headingish = |l: &crate::ocr::OcrLine| {
        let text = l.text();
        let text = text.trim();
        let letters = text.chars().filter(|c| c.is_alphabetic()).count();
        let len = text.chars().count();
        (3..=60).contains(&len) && letters * 10 >= len * 6 && !NOT_A_TITLE.is_match(text) && find_dates(text).is_empty()
    };
    let best = page
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.bbox.y < page_h * 0.45 && headingish(l))
        .map(|(i, l)| (l.bbox.h / median * 2.0 + 1.0 - l.bbox.y / page_h, i))
        .max_by(|a, b| a.0.total_cmp(&b.0))?
        .1;
    // Logos and big headings often wrap ("Harbor View" / "Suites"): take
    // the neighbouring lines that are just as big, stacked right on top.
    let stacked = |a: &crate::ocr::OcrLine, b: &crate::ocr::OcrLine| {
        let ratio = a.bbox.h / b.bbox.h.max(1.0);
        let gap = b.bbox.y - (a.bbox.y + a.bbox.h);
        let overlap = a.bbox.x < b.bbox.x + b.bbox.w && b.bbox.x < a.bbox.x + a.bbox.w;
        (0.6..=1.7).contains(&ratio) && gap < 0.8 * a.bbox.h.max(b.bbox.h) && gap > -0.5 * a.bbox.h && overlap
    };
    let (mut first, mut last) = (best, best);
    while first > 0 && headingish(&page.lines[first - 1]) && stacked(&page.lines[first - 1], &page.lines[first]) {
        first -= 1;
    }
    while last + 1 < page.lines.len()
        && headingish(&page.lines[last + 1])
        && stacked(&page.lines[last], &page.lines[last + 1])
    {
        last += 1;
    }
    Some(page.lines[first..=last].iter().map(|l| l.text().trim().to_string()).collect::<Vec<_>>().join(" "))
}

pub fn suggest_heuristic(pages: &[&OcrPage]) -> Option<Suggestion> {
    let first = pages.first()?;
    // The first two pages carry the identifying information; later pages
    // only add noise (and time).
    let text: String = pages.iter().take(2).map(|p| p.text()).collect::<Vec<_>>().join("\n");
    if text.trim().is_empty() {
        return None;
    }
    let heading = best_heading(first);
    let doc_type = best_type(&text, heading.as_deref());
    let doc_date = best_date(&text);
    // A heading that is just the type word ("INVOICE") isn't a title.
    let title = heading
        .filter(|h| doc_type.as_ref().is_none_or(|t| !h.trim().eq_ignore_ascii_case(t)))
        .map(|h| tidy_case(&h).split_whitespace().take(6).collect::<Vec<_>>().join(" "));
    let name = match (&title, &doc_type) {
        (Some(t), Some(ty)) if !t.to_lowercase().contains(&ty.to_lowercase()) => format!("{t} {ty}"),
        (Some(t), _) => t.clone(),
        (None, Some(ty)) => ty.clone(),
        (None, None) => return None,
    };
    Some(Suggestion { name: sanitize(&name), title, doc_type, doc_date })
}

// ─── Local LLM ─────────────────────────────────────────────────────────────

const PROMPT: &str = "You file scanned documents. From the OCR text below, reply with ONLY a JSON object: \
{\"title\": sender or subject in 1-5 words, \"type\": document type in 1-2 words (e.g. Invoice, Statement, Letter), \
\"date\": the document's own date as YYYY-MM-DD or null, \"name\": a concise file name of 2-6 words without date or extension}.\n\nOCR text:\n";

#[derive(Deserialize)]
struct LlmReply {
    title: Option<String>,
    #[serde(rename = "type")]
    doc_type: Option<String>,
    date: Option<String>,
    name: Option<String>,
}

pub fn suggest_llm(pages: &[&OcrPage], settings: &NamingSettings) -> Result<Suggestion> {
    let text: String = pages.iter().take(2).map(|p| p.text()).collect::<Vec<_>>().join("\n");
    let text: String = text.chars().take(4000).collect();
    let prompt = format!("{PROMPT}{text}");
    let base = settings.llm_endpoint.trim_end_matches('/');
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(90))).build().into();

    let content = match settings.llm_api {
        LlmApi::Ollama => {
            let body = serde_json::json!({ "model": settings.llm_model, "prompt": prompt, "stream": false, "format": "json", "options": { "temperature": 0 } });
            let reply: serde_json::Value =
                agent.post(&format!("{base}/api/generate")).send_json(&body)?.body_mut().read_json()?;
            reply["response"].as_str().context("no response field")?.to_string()
        }
        LlmApi::OpenAi => {
            let url = if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/v1/chat/completions")
            };
            let body = serde_json::json!({ "model": settings.llm_model, "temperature": 0, "messages": [{ "role": "user", "content": prompt }] });
            let reply: serde_json::Value = agent.post(&url).send_json(&body)?.body_mut().read_json()?;
            reply["choices"][0]["message"]["content"].as_str().context("no message content")?.to_string()
        }
    };
    parse_llm_reply(&content)
}

fn parse_llm_reply(content: &str) -> Result<Suggestion> {
    // Models wrap JSON in prose or code fences often enough to be worth
    // tolerating: take the outermost {...}.
    let start = content.find('{').ok_or_else(|| anyhow!("no JSON in reply"))?;
    let end = content.rfind('}').ok_or_else(|| anyhow!("no JSON in reply"))?;
    let reply: LlmReply = serde_json::from_str(&content[start..=end])?;
    let clean = |s: Option<String>| s.map(|s| sanitize(&s)).filter(|s| !s.is_empty() && s != "null");
    let title = clean(reply.title);
    let doc_type = clean(reply.doc_type);
    let doc_date = reply.date.and_then(|d| d.parse::<Date>().ok());
    let name = clean(reply.name)
        .or_else(|| title.clone())
        .or_else(|| doc_type.clone())
        .ok_or_else(|| anyhow!("model gave no name"))?;
    Ok(Suggestion { name, title, doc_type, doc_date })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::{OcrLine, OcrWord, Rect};

    fn page(lines: &[(&str, f32, f32)]) -> OcrPage {
        OcrPage {
            width: 2550,
            height: 3300,
            lines: lines
                .iter()
                .map(|(t, y, h)| {
                    let bbox = Rect { x: 200.0, y: *y, w: 800.0, h: *h };
                    OcrLine { words: t.split(' ').map(|w| OcrWord { text: w.into(), bbox }).collect(), bbox }
                })
                .collect(),
        }
    }

    fn ctx<'a>(s: Option<&'a Suggestion>) -> NameContext<'a> {
        NameContext {
            now: "2026-10-20T14:05:09-04:00[America/New_York]".parse().unwrap(),
            suggestion: s,
            counter: 7,
            pages: 3,
            profile: "Docs",
            scanner: "ES-300W",
        }
    }

    #[test]
    fn renders_placeholders() {
        let s = Suggestion {
            name: "Acme Invoice".into(),
            title: Some("Acme".into()),
            doc_type: Some("Invoice".into()),
            doc_date: Some(Date::new(2026, 3, 9).unwrap()),
        };
        assert_eq!(render("{date} {suggested}", &ctx(Some(&s))), PathBuf::from("2026-10-20 Acme Invoice"));
        assert_eq!(
            render("{type}/{doc_date:%d.%m.%Y} {title}", &ctx(Some(&s))),
            PathBuf::from("Invoice/09.03.2026 Acme")
        );
        assert_eq!(render("Scan {counter:4} ({pages}p)", &ctx(None)), PathBuf::from("Scan 0007 (3p)"));
        assert_eq!(render("{suggested}", &ctx(None)), PathBuf::from("Scan"));
        assert_eq!(render("{datetime}", &ctx(None)), PathBuf::from("2026-10-20 14-05-09"));
    }

    #[test]
    fn sanitizes_components() {
        assert_eq!(sanitize("  a:b*c?  "), "a b c");
        assert_eq!(sanitize("con"), "con_");
        assert_eq!(sanitize("..hidden.."), "hidden");
        assert_eq!(render("../../{title}", &ctx(None)), PathBuf::from("Scan"));
    }

    #[test]
    fn finds_dates_in_many_formats() {
        let dates: Vec<Date> = find_dates("on 2026-03-09, 03/10/2026, 25.12.2025, March 4th, 2026 and 7 Feb 2025")
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(
            dates,
            vec![
                Date::new(2026, 3, 9).unwrap(),
                Date::new(2026, 3, 10).unwrap(),
                Date::new(2025, 12, 25).unwrap(),
                Date::new(2026, 3, 4).unwrap(),
                Date::new(2025, 2, 7).unwrap(),
            ]
        );
    }

    #[test]
    fn prefers_labelled_date_over_due_date() {
        let text = "Payment due 11/15/2026\nStatement date: 10/01/2026";
        assert_eq!(best_date(text), Some(Date::new(2026, 10, 1).unwrap()));
    }

    #[test]
    fn suggests_from_a_statement() {
        let p = page(&[
            ("COMCAST BUSINESS", 150.0, 90.0),
            ("1701 John F Kennedy Blvd", 260.0, 35.0),
            ("Statement date: October 1, 2026", 400.0, 35.0),
            ("Account summary", 500.0, 40.0),
            ("Beginning balance $120.00", 560.0, 35.0),
            ("Amount due $99.00 Pay by 10/21/2026", 620.0, 35.0),
        ]);
        let s = suggest_heuristic(&[&p]).unwrap();
        assert_eq!(s.doc_type.as_deref(), Some("Statement"));
        assert_eq!(s.title.as_deref(), Some("Comcast Business"));
        assert_eq!(s.name, "Comcast Business Statement");
        assert_eq!(s.doc_date, Some(Date::new(2026, 10, 1).unwrap()));
    }

    #[test]
    fn joins_a_heading_wrapped_over_two_lines() {
        let p = page(&[
            ("Harbor View", 120.0, 80.0),
            ("Suites", 210.0, 85.0),
            ("Folio No. 10482", 400.0, 30.0),
            ("Total 212.40 Balance 0.00", 900.0, 30.0),
            ("Visa 212.40", 950.0, 30.0),
            ("Guest Signature", 1000.0, 30.0),
        ]);
        let s = suggest_heuristic(&[&p]).unwrap();
        assert_eq!(s.title.as_deref(), Some("Harbor View Suites"));
        assert_eq!(s.doc_type.as_deref(), Some("Receipt"));
    }

    #[test]
    fn names_a_medical_receipt_by_its_own_date() {
        // Laid out like a real ES-300W scan of a medical receipt (contents
        // made up): the receipt date sits by the patient block, the date of
        // the visit it pays for further down in the line items.
        let p = page(&[
            ("Lakeside Family Clinic", 300.0, 45.0),
            ("Guarantor Patient", 400.0, 30.0),
            ("Jane Sample #100200 Jane Sample", 440.0, 35.0),
            ("3/14/26", 500.0, 35.0),
            ("Previous Balance", 750.0, 35.0),
            ("Past Professional Services", 790.0, 35.0),
            ("Acct #55501234 - Office Visit on 02/02/26 $120.00", 830.0, 35.0),
            ("Total Paid", 950.0, 30.0),
            ("$120.00", 1000.0, 70.0),
            ("Visa x0000", 1200.0, 35.0),
            ("Signature:", 1350.0, 35.0),
            ("PATIENT COPY", 1480.0, 35.0),
            ("Please keep this receipt for your records.", 1620.0, 35.0),
        ]);
        let s = suggest_heuristic(&[&p]).unwrap();
        assert_eq!(s.doc_date, Some(Date::new(2026, 3, 14).unwrap()));
        assert_eq!(s.doc_type.as_deref(), Some("Medical Receipt"));
        assert_eq!(s.name, "Lakeside Family Clinic Medical Receipt");
        let ctx = NameContext { suggestion: Some(&s), ..ctx(None) };
        assert_eq!(
            render(crate::config::DEFAULT_NAME_TEMPLATE, &ctx),
            PathBuf::from("2026-03-14 Lakeside Family Clinic Medical Receipt")
        );
    }

    #[test]
    fn finds_hyphenated_month_name_dates() {
        let dates: Vec<Date> = find_dates("Issued 19-Aug-2026, due Sep-18-2026").into_iter().map(|(_, d)| d).collect();
        assert_eq!(dates, vec![Date::new(2026, 8, 19).unwrap(), Date::new(2026, 9, 18).unwrap()]);
    }

    #[test]
    fn type_only_heading_is_not_a_title() {
        let p =
            page(&[("INVOICE", 150.0, 100.0), ("Invoice number 1234", 400.0, 30.0), ("Bill to: Jane", 450.0, 30.0)]);
        let s = suggest_heuristic(&[&p]).unwrap();
        assert_eq!(s.name, "Invoice");
    }

    #[test]
    fn parses_chatty_llm_reply() {
        let s = parse_llm_reply("Sure!\n```json\n{\"title\":\"Acme Corp\",\"type\":\"Invoice\",\"date\":\"2026-02-01\",\"name\":\"Acme Invoice February\"}\n```").unwrap();
        assert_eq!(s.name, "Acme Invoice February");
        assert_eq!(s.doc_date, Some(Date::new(2026, 2, 1).unwrap()));
    }
}
