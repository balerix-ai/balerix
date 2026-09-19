//! `question_plan <tool_input.json> <reply>`: what the matrix plugin would
//! do with `reply` while that `AskUserQuestion` is open, as JSON. Used by
//! `scripts/verify-questions.sh` to drive the real `claude` with the
//! plugin's own key plans (Spec J §10).

use balerix_plugin_matrix::question::{self, Matched, Refusal};
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(path), Some(reply)) = (args.next(), args.next()) else {
        anyhow::bail!("usage: question_plan <tool_input.json> <reply>");
    };
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let questions = question::parse(&input)
        .ok_or_else(|| anyhow::anyhow!("{path}: not an AskUserQuestion tool_input"))?;
    let out = match question::match_reply(&questions, &reply) {
        Ok(Matched::Answers { selections, exact }) => json!({
            "steps": question::plan(&questions, &selections),
            "exact": exact,
            "chosen": question::describe(&questions, &selections),
        }),
        Ok(Matched::Skip) => json!({
            "steps": question::skip_plan(),
            "exact": true,
            "chosen": "skip",
        }),
        Err(Refusal(reason)) => anyhow::bail!("refused: {reason}"),
    };
    println!("{out}");
    Ok(())
}
