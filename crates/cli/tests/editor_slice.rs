//! Scripted coverage for the first linked-media editor slice.

use std::process::Command;

#[test]
fn script_and_interactive_engine_share_recursive_media_commands() {
    let script =
        std::env::temp_dir().join(format!("mmrecode-editor-slice-{}.mmrs", std::process::id()));
    std::fs::write(
        &script,
        "add video Clip0 100f\n\
         cd Clip0\n\
         add text \"Opening Title\" 20f at 10f\n\
         ls\n\
         cd \"Opening Title\"\n\
         in +5f\n\
         info\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mmrecode"))
        .arg("edit")
        .arg(&script)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&script);

    assert!(
        output.status.success(),
        "editor script failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Opening Title    [text"));
    assert!(stdout.contains("ok: in +5f  [/Clip0/Opening Title]"));
    assert!(stdout.contains("source: 5f..20f"));
    assert!(stdout.contains("parent timeline: 15f..30f"));
}
