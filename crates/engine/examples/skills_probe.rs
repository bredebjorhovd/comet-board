//! Dev probe for the composer's `/` picker (gh#134): what `ListSkills` would
//! offer for a given config dir and cwd, without launching a viewport.
//!
//! ```text
//! cargo run -p comet-engine --example skills_probe -- ~/.claude . /com
//! cargo run -p comet-engine --example skills_probe -- ~/Library/…/accounts/<slot> /repo
//! ```
//!
//! The second form is the one worth knowing: point it at an agent account's
//! materialized dir and it shows what a *dispatched* run can actually invoke,
//! which is not what the box user's shell would find (§gh#59).
//! With a `/query` it also runs the composer's own trigger and filter, so what
//! it prints is exactly the menu the picker would show.
fn main() {
    let mut args = std::env::args().skip(1);
    let config_dir = args.next().map(std::path::PathBuf::from);
    let cwd = args.next().map(std::path::PathBuf::from);
    let query = args.next();
    let rows = comet_engine::skills::list(&comet_engine::skills::SkillRoots { config_dir, cwd });
    let rows = match query {
        Some(typed) => {
            use comet_proto::view::skills;
            let token = skills::slash_token(&typed, typed.len())
                .unwrap_or_else(|| panic!("`{typed}` does not open a picker"));
            let filtered = skills::filter(&token.query, &rows);
            if let Some(top) = filtered.first() {
                let (completed, caret) = skills::complete(&typed, &token, &top.name);
                println!("enter completes to {completed:?} (caret {caret})");
            }
            filtered
        }
        None => rows,
    };
    println!("{} skills", rows.len());
    for row in rows.iter().take(200) {
        println!(
            "  /{:<28} {:<8} {}",
            row.name,
            row.source.label(),
            row.description.as_deref().unwrap_or("")
        );
    }
}
