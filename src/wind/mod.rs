use std::time::{SystemTime, UNIX_EPOCH};

use crate::cli::WindArgs;
use crate::kiteops::print_command_line;

const KITE_EMOJI: &str = "\u{1FA81}";

const ENCOURAGEMENTS: &[&str] = &[
    "Wind is not always behind you, but lift can still be.",
    "Small progress still catches air.",
    "Hold the line steady; the sky will do the rest.",
    "Even a quiet breeze can carry you farther than fear.",
    "Today is enough for one more rise.",
    "Tension and motion can work together.",
    "The sky does not ask for perfection, only balance.",
    "When the wind shifts, adjust the string, not the dream.",
    "A steady hand can turn a gust into direction.",
    "You do not need a storm to move forward.",
    "Clarity often arrives after one more step.",
    "A patient climb is still a climb.",
    "Good work can start from a small, quiet try.",
    "What feels fragile can still stay aloft.",
    "Your next attempt does not need permission from doubt.",
    "There is room in today for one clean decision.",
    "Momentum likes consistency more than drama.",
    "You can correct course without starting over.",
    "Some days the win is simply not letting go.",
    "The line may pull hard because you are actually rising.",
    "It is fine to move carefully and still move well.",
    "A little more sky opens after a little more patience.",
    "Trust the craft you have already built.",
    "Not every gust is resistance; some of it is lift.",
    "You are allowed to improve in public.",
    "Calm effort travels farther than panic.",
    "Keep the frame light and the purpose firm.",
    "One useful pass is better than ten worried circles.",
    "You can be in motion before you feel ready.",
    "A better angle can change the whole wind.",
    "The next clean line is enough for now.",
    "Quiet confidence still counts as strength.",
    "Even small altitude changes improve the view.",
    "What holds steady matters as much as what moves fast.",
    "The sky keeps opening for people who keep adjusting.",
    "You do not need certainty to catch a current.",
    "Progress is often just balance repeated well.",
    "Let the breeze meet you halfway.",
];

pub fn run_wind(_args: &WindArgs) -> anyhow::Result<()> {
    print_command_line("wind");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize)
        .unwrap_or(0);
    let pid = std::process::id() as usize;
    let index = nanos.wrapping_add(pid) % ENCOURAGEMENTS.len();

    println!("{KITE_EMOJI} {}", ENCOURAGEMENTS[index]);
    Ok(())
}
