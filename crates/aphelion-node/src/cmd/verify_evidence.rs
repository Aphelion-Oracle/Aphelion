//! `verify-evidence`: check a bundle somebody else produced.
//!
//! The one command in this binary that needs nothing. No configuration, no key,
//! no database, no chain, no network — a committee member judging a dispute has
//! none of those for the node they are judging, and a verifier that required any
//! of them would be a verifier only the accused could run.
//!
//! What it does take, optionally, is the allegation: `--node`, `--feed`,
//! `--nonce`, `--aggregator` and `--digest`, as `dispute show` prints them.
//! Those are typed by the person checking rather than read from the file, which
//! is the whole point of them — a bundle asked to supply the standard it is
//! measured against will meet it.
//!
//! `--digest` is the odd one and the one a committee should reach for first. The
//! other four are compared against the signed payload; this one is compared
//! against the file's own bytes, and it answers a question the payload cannot:
//! whether this is the document the accused committed to on the ledger while
//! the vote was open, or one that arrived afterwards.
//!
//! The judgement is [`aphelion_node::engine::verify`]. What is here is reading a
//! file, turning five flags into [`Expectations`], and printing the result.

use std::io::{Read, Write};
use std::path::Path;

use aphelion_node::engine::verify::{verify_document, Audit, Expectations, Verdict};
use aphelion_node::error::{NodeError, Result};

/// The allegation, as the caller typed it off the dispute.
#[derive(Debug, Clone, Default)]
pub struct Against {
    pub node: Option<String>,
    pub feed: Option<String>,
    pub nonce: Option<u64>,
    pub aggregator: Option<String>,
    /// SHA-256 of the answer the accused put on the record, from
    /// `slashing.responses` — `dispute show` prints it.
    pub digest: Option<String>,
}

/// `EX_USAGE`, as `sysexits.h` has meant it for forty years.
///
/// Not 1, and not any of [`Verdict::exit_code`]'s three. This command's exit
/// status is read by scripts as a judgement on a bundle, so a mistake by the
/// person running it has to land outside that range — 1 would be read as
/// `unsupported`, which is a finding against the accused and the opposite of
/// what happened.
const EX_USAGE: i32 = 64;

/// Read the bundle from a path or from stdin, audit it, print, exit.
pub fn run(path: &Path, against: &Against, json: bool) -> Result<()> {
    // Parsed first, and separately from the audit. A mistyped key is the
    // caller's mistake; reported as a verdict it would read as a finding
    // against the accused. Reported through `main` it would exit 1, which is
    // the same thing one layer down, so it exits here instead.
    let expect = Expectations::parse(
        against.node.as_deref(),
        against.feed.as_deref(),
        against.nonce,
        against.aggregator.as_deref(),
        against.digest.as_deref(),
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(EX_USAGE);
    });

    // Bytes, not a string, and never re-serialised on the way through. The
    // digest the accused published is over the file exactly as it is, so
    // anything that normalised it here — trailing newlines, re-encoding — would
    // be quietly answering a different question from the one `--digest` asks.
    let raw = if path == Path::new("-") {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf).map_err(|e| {
            NodeError::Other(anyhow::anyhow!("cannot read the bundle from stdin: {e}"))
        })?;
        buf
    } else {
        std::fs::read(path).map_err(|e| {
            NodeError::Other(anyhow::anyhow!("cannot read `{}`: {e}", path.display()))
        })?
    };

    // A file that will not parse is not a failed audit, it is not a bundle. The
    // distinction matters: a verdict of any kind implies something was judged.
    // Deliberately not a `Config` error: nothing about this operator's setup is
    // wrong, and `main` appends a pointer to the setup guide for that variant.
    let audit = verify_document(&raw, &expect).map_err(|e| {
        NodeError::Other(anyhow::anyhow!(
            "this is not an Aphelion evidence bundle: {e}. A bundle is the output of \
             `aphelion-node replay <feed> <nonce> --json`."
        ))
    })?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&audit).map_err(|e| NodeError::Other(e.into()))?
        );
    } else {
        render(&audit);
    }

    std::io::stdout().flush().ok();
    std::process::exit(audit.verdict.exit_code());
}

fn render(a: &Audit) {
    match &a.signed {
        None => {
            println!("Signed payload");
            println!("  nothing established — see below");
        }
        Some(s) => {
            // Deliberately labelled "signed", not "claimed". Everything in this
            // block came out of the bytes the signature covers; nothing in it
            // was read from the bundle's prose.
            println!("Signed payload");
            println!("  feed          {}", s.feed);
            println!("  price         {}", s.price);
            println!("  confidence    {} bps", s.confidence_bps);
            println!("  observed at   {}", s.timestamp);
            println!("  nonce         {}", s.nonce);
            println!("  aggregator    {}", s.aggregator);
            println!("  key           {}", s.public_key);
        }
    }
    println!();

    if let Some(d) = &a.document_digest {
        // Printed on every audit, not only when one was given. This is the
        // number the accused publishes with `dispute respond`, and the number a
        // committee compares against `slashing.responses`.
        println!("Document SHA-256: {d}");
        println!();
    }

    println!("Observations offered: {}", a.observation_count);
    if let Some(p) = &a.recomputed {
        println!("  they produce:      {p}");
    }
    println!();

    println!("Verdict: {}", verdict_line(a.verdict));
    for f in &a.findings {
        println!("  [{}] {}", f.verdict, wrap(&f.detail, 4));
    }

    if a.signed.is_some() {
        println!();
        // What remains outside this command's reach depends on what it was
        // given. Without the allegation it is the whole question of relevance;
        // with it, the narrower one the registry answers.
        let remaining = match a.bound_to_allegation {
            None => {
                "Still to establish elsewhere: that this bundle is about the dispute at \
                 all. Read the accused, feed and nonce off `dispute show` and pass them \
                 as --node, --feed and --nonce, and they are checked against the signed \
                 bytes here."
            }
            Some(_) => {
                "Still to establish elsewhere: that the key the allegation names is the \
                 key of the operator it is against. That is `registry.owner_of`, and no \
                 bundle can settle it."
            }
        };
        println!("{}", wrap(remaining, 0));
    }
}

fn verdict_line(v: Verdict) -> &'static str {
    match v {
        Verdict::Sound => {
            "sound — signed, honestly described, and supported by the observations offered"
        }
        Verdict::Unsupported => {
            "unsupported — genuinely signed, but the observations offered do not produce it"
        }
        Verdict::Unrelated => "unrelated — genuine, and about something other than this allegation",
        Verdict::Misdescribed => {
            "misdescribed — the signature is good and the bundle describes something else"
        }
        Verdict::Unsigned => "unsigned — the bundle establishes nothing",
    }
}

/// Wrap to something readable in a terminal, indented under its tag.
fn wrap(text: &str, indent: usize) -> String {
    const WIDTH: usize = 76;
    let pad = " ".repeat(indent);
    let mut out = String::new();
    let mut line = 0usize;
    for word in text.split_whitespace() {
        if line > 0 && line + 1 + word.len() > WIDTH {
            out.push('\n');
            out.push_str(&pad);
            line = 0;
        } else if line > 0 {
            out.push(' ');
            line += 1;
        }
        out.push_str(word);
        line += word.len();
    }
    out
}
