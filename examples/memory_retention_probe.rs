//! Local tokenizer retention probe; no model requests or project writes.
//! Build with `cargo build --example memory_retention_probe`, then compare
//! peak RSS for 1 and 6 model aliases using the platform's process profiler.
fn main() {
    let aliases = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>().expect("positive alias count"))
        .unwrap_or(6);
    assert!(aliases > 0);
    let text = "메모리 수명 검증: source evidence and cancellation cleanup";
    let mut total = 0;
    for index in 0..aliases {
        total += mnemoarc::context::tokens(text, &format!("gpt-4o-retention-probe-{index}"));
    }
    println!("model aliases: {aliases}, total tokens: {total}");
}
