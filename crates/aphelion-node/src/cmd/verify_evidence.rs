//! `verify-evidence`: check a bundle somebody else produced.
//!
//! The one command in this binary that needs nothing. No configuration, no key,
//! no database, no chain, no network — a committee member judging a dispute has
//! none of those for the node they are judging, and a verifier that required any
//! of them would be a verifier only the accused could run.
//!
//! The judgement is [`aphelion_node::engine::verify`]. What is here is reading a
//! file and printing the result.

use std::io::{Read, Write};
use std::path::Path;

use aphelion_node::engine::verify::{verify, Audit, Bundle, Verdict};
use aphelion_node::error::{NodeError, Result};

/// Read the bundle from a path or from stdin, audit it, print, exit.
pub fn run(path: &Path, json: bool) -> Result<()> {
    let raw = if path == Path::new("-") {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map_err(|e| {
            NodeError::Other(anyhow::anyhow!("cannot read the bundle from stdin: {e}"))
        })?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            NodeError::Other(anyhow::anyhow!("cannot read `{}`: {e}", path.display()))
        })?
    };

    // A file that will not parse is not a failed audit, it is not a bundle. The
    // distinction matters: a verdict of any kind implies something was judged.
    // Deliberately not a `Config` error: nothing about this operator's setup is
    // wrong, and `main` appends a pointer to the setup guide for that variant.
    let bundle: Bundle = serde_json::from_str(&raw).map_err(|e| {
        NodeError::Other(anyhow::anyhow!(
            "this is not an Aphelion evidence bundle: {e}. A bundle is the output of \
             `aphelion-node replay <feed> <nonce> --json`."
        ))
    })?;

    let audit = verify(&bundle);

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
        println!(
            "{}",
            wrap(
                "Still to establish elsewhere: that the key above is the key this dispute is \
                 about. The registry answers that; a bundle cannot, because a sound bundle \
                 about another node is still a sound bundle.",
                0
            )
        );
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
