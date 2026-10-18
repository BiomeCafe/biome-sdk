//! What `restart_through_biome_if_needed` would do for this program, without
//! doing it: prints the facts and the decision, starts nothing.
//!
//! ```text
//! cargo run -p biome-online-services --release --example relaunch_check -- omg
//! cargo run -p biome-online-services --release --example relaunch_check -- mecha-aether --as "C:\Users\me\AppData\Local\Biome\games\mecha-aether\0.4.0\mecha_aether.exe"
//! ```
//!
//! `--as <path>` decides for that program instead of this one (still read
//! only), to see the answer for a build sitting in Biome's games folder.

use std::path::{Path, PathBuf};

use biome_online_services::relaunch::{decide, read_install_record, run_url, RelaunchFacts};

fn main() {
    let mut args = std::env::args().skip(1);
    let app_id = args.next().unwrap_or_else(|| "omg".into());
    let mut facts = RelaunchFacts::gather();
    while let Some(arg) = args.next() {
        if arg == "--as" {
            let path = PathBuf::from(args.next().expect("--as <path>"));
            facts.exe = Some(biome_online_services::relaunch::canonical(&path).unwrap_or(path));
        }
    }
    println!("app id:            {app_id}");
    println!("debug build:       {}", facts.debug_build);
    println!("opted out:         {}", facts.opted_out);
    println!("launched by Biome: {}", facts.launched_by_biome);
    println!("Biome launcher:    {:?}", facts.launcher);
    println!("takes run links:   {}", facts.run_links);
    println!("program:           {:?}", facts.exe);
    match decide(&app_id, &facts, read_install_record, Path::is_file) {
        biome_online_services::relaunch::Relaunch::Through { launcher } => {
            println!("decision:          hand {} to {} and exit", run_url(&app_id), launcher.display())
        }
        biome_online_services::relaunch::Relaunch::Skip(why) => println!("decision:          run as is ({why:?})"),
    }
}
