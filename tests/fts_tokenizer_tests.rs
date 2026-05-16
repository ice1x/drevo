use drevo::fts::{extract_trigrams, normalize, trigrams};

// --- Realistic use-case texts ---

#[test]
fn cbt_journal_entry() {
    let title = "Anxious thought about meeting";
    let body = "I felt overwhelmed before the team standup. \
                My heart was racing and I couldn't focus on my notes. \
                Cognitive distortion: catastrophizing.";
    let result = extract_trigrams(title, body);
    assert!(!result.is_empty());
    // Should contain trigrams from key terms
    assert!(result.contains(&"anx".to_string())); // "anxious"
    assert!(result.contains(&"mee".to_string())); // "meeting"
    assert!(result.contains(&"cat".to_string())); // "catastrophizing"
}

#[test]
fn story_editor_scene() {
    let title = "Chapter 3: The Dark Forest";
    let body = "Elena stepped through the ancient gate, her torch flickering \
                against moss-covered walls. The sound of dripping water echoed \
                in the narrow passage ahead.";
    let result = extract_trigrams(title, body);
    assert!(!result.is_empty());
    assert!(result.contains(&"for".to_string())); // "forest"
    assert!(result.contains(&"ele".to_string())); // "elena"
    assert!(result.contains(&"tor".to_string())); // "torch"
}

#[test]
fn task_manager_ticket() {
    let title = "Fix authentication timeout bug";
    let body = "Users report 401 errors after 15 minutes of inactivity. \
                The JWT refresh token endpoint returns 500 intermittently. \
                Priority: P1, Sprint: 2024-Q1-W3.";
    let result = extract_trigrams(title, body);
    assert!(!result.is_empty());
    assert!(result.contains(&"aut".to_string())); // "authentication"
    assert!(result.contains(&"bug".to_string())); // "bug"
    assert!(result.contains(&"jwt".to_string())); // "jwt"
}

#[test]
fn erp_order_description() {
    let title = "Purchase Order #12345";
    let body = "Supplier: Acme Corp. Items: 500x Widget A ($2.50 each), \
                200x Gadget B ($15.00 each). Warehouse: Building 7, Shelf C3.";
    let result = extract_trigrams(title, body);
    assert!(!result.is_empty());
    assert!(result.contains(&"pur".to_string())); // "purchase"
    assert!(result.contains(&"acm".to_string())); // "acme"
}

#[test]
fn bug_tracker_report() {
    let title = "Memory leak in WebSocket handler";
    let body = "After 10,000 connections the process RSS grows to 4GB. \
                Heap dump shows unreleased buffers in ws::Connection::read_frame. \
                Reproduces on Linux x86_64 only.";
    let result = extract_trigrams(title, body);
    assert!(!result.is_empty());
    assert!(result.contains(&"mem".to_string())); // "memory"
    assert!(result.contains(&"lea".to_string())); // "leak"
    assert!(result.contains(&"web".to_string())); // "websocket"
}

// --- Edge cases ---

#[test]
fn all_punctuation_input() {
    let result = trigrams("!@#$%^&*()_+-=[]{}|;':\",./<>?");
    assert!(result.is_empty());
}

#[test]
fn all_whitespace_input() {
    let result = trigrams("     \t\n\r   ");
    assert!(result.is_empty());
}

#[test]
fn single_character() {
    assert!(trigrams("a").is_empty());
}

#[test]
fn emoji_input() {
    // Emoji are not alphanumeric and not CJK, so they get stripped
    let result = normalize("hello 🌍 world 🎉");
    assert_eq!(result, "hello world");
}

#[test]
fn mixed_scripts() {
    let result = trigrams("hello世界test");
    assert!(!result.is_empty());
    // Should contain Latin trigrams and CJK bigrams
    assert!(result.contains(&"hel".to_string()));
    // CJK bigram from adjacent CJK chars
    // After normalization: "hello世界test"
    // The "世界" pair should appear
    assert!(result.contains(&"世界".to_string()));
}

#[test]
fn large_text_no_panic() {
    let large = "a ".repeat(50_000); // 100K chars
    let result = trigrams(&large);
    // Should not panic or stack overflow
    assert!(!result.is_empty());
}

#[test]
fn repeated_text_deduplication() {
    let text = "the the the the the";
    let result = trigrams(text);
    // Should be heavily deduplicated
    assert!(result.len() < 10);
    assert!(result.contains(&"the".to_string()));
}

#[test]
fn numbers_in_text() {
    let result = trigrams("test123abc");
    assert!(result.contains(&"tes".to_string()));
    assert!(result.contains(&"123".to_string()));
    assert!(result.contains(&"abc".to_string()));
}

#[test]
fn markdown_content() {
    let body = "## Heading\n\n- bullet point\n- **bold** text\n\n```rust\nfn main() {}\n```";
    let result = trigrams(body);
    assert!(!result.is_empty());
    // Markdown syntax gets stripped (punctuation), content survives
    assert!(result.contains(&"hea".to_string())); // "heading"
    assert!(result.contains(&"bol".to_string())); // "bold"
}

#[test]
fn unicode_accented_characters() {
    let result = normalize("café résumé naïve");
    assert_eq!(result, "café résumé naïve");
    let tris = trigrams("café");
    assert!(tris.contains(&"caf".to_string()));
    assert!(tris.contains(&"afé".to_string()));
}

#[test]
fn japanese_hiragana_text() {
    let result = trigrams("こんにちは");
    // 5 hiragana chars -> trigrams + bigrams
    assert!(result.contains(&"こん".to_string())); // bigram
    assert!(result.contains(&"こんに".to_string())); // trigram
}

#[test]
fn korean_hangul_text() {
    let result = trigrams("안녕하세요");
    assert!(result.contains(&"안녕".to_string())); // bigram
    assert!(result.contains(&"안녕하".to_string())); // trigram
}

#[test]
fn extract_trigrams_title_body_boundary() {
    // Trigrams should span across the title-body boundary
    let result = extract_trigrams("end", "start");
    // "end start" -> includes "d s", "nd ", " st" etc.
    assert!(result.contains(&"nd ".to_string()));
    assert!(result.contains(&" st".to_string()));
}
