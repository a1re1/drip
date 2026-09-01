// bin `drip` — port of src/cli/main.tsx (dispatch).
//
// Order mirrors main.tsx:613-644: parse (usage errors → stderr, exit 1),
// then --help, then --version. Everything else is not ported yet and says so
// with the same shape lci uses for its own unimplemented surfaces.

use drip::cli::args::parse_cli_args;
use drip::cli::help::HELP;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli_args = parse_cli_args(&argv);

    // src/cli/main.tsx:616-622 — fatal usage problems are printed and the
    // process exits 1 instead of guessing.
    if !cli_args.errors.is_empty() {
        for problem in &cli_args.errors {
            eprintln!("{}", problem);
        }

        std::process::exit(1);
    }

    if cli_args.help {
        // lci: console.log(CLI_HELP_TEXT) — console.log appends one newline and
        // the template itself ends with one, so stdout ends "\n\n"; println!
        // over HELP reproduces that exactly.
        println!("{}", HELP);
        return;
    }

    if cli_args.version {
        // lci reads package.json at runtime; drip embeds the Cargo package
        // version at compile time (port brief: drip reports its own version).
        let version = env!("CARGO_PKG_VERSION");

        if cli_args.json {
            // JSON.stringify({ version }) — exact bytes, no spaces.
            println!("{{\"version\":\"{}\"}}", version);
            return;
        }

        println!("drip {}", version);
        return;
    }

    // drip-only: lci → drip migration (`--migrate-from-lci [--from <dir>]
    // [--dry-run] [--project]`) — drip/PLAN.md; there is no TS dispatch to
    // mirror, so it sits after help/version like the other leaf commands.
    if cli_args.migrate_from_lci {
        let options = drip::migrate::MigrateOptions {
            from: cli_args.migrate_from.clone(),
            dry_run: cli_args.dry_run,
            project: cli_args.migrate_project,
            to: None,
            project_root: None,
        };

        return match drip::migrate::migrate_from_lci(&options) {
            Ok(report) => {
                // Summary table on stdout; exit 0 even when every entry was
                // skipped (the migration is idempotent, not an error).
                print!("{}", report.summary());
            }
            // Missing --from, unwritable destination, etc. — one clear line
            // on stderr, exit 1.
            Err(err) => {
                eprintln!("{err:#}");
                std::process::exit(1);
            }
        };
    }

    // Remaining dispatch (goal runs, subcommands, flags beyond help/version)
    // lands with the later port waves (core state, harness, tools). Echo the
    // leading token back as the unported "command" (brief: `drip: <command>
    // is not ported yet`); a bare `drip` with no argv has no command to name.
    match argv.first() {
        Some(command) => eprintln!("drip: {} is not ported yet", command),
        None => eprintln!("drip: not ported yet"),
    }

    std::process::exit(1);
}
