//! A stand-in for the `stellar` CLI.
//!
//! The node shells out to `stellar contract invoke` for every chain call and
//! takes the binary's name from `APHELION_STELLAR_BIN`. Pointing that at this
//! program is the entire seam the harness needs: the node under test is the
//! real, unmodified `aphelion-node` binary, doing real process spawns, and it
//! cannot tell that the chain on the other side is a test fixture.
//!
//! Everything this understands is dictated by `chain::cli`: the argument
//! shape it builds, the JSON it expects on stdout, the transaction hash it
//! scrapes from stderr, and the non-zero exit it reads as "the chain said no".
//! Deviating from any of those would make the harness pass against a node the
//! real CLI would break.

use std::collections::HashMap;
use std::process::ExitCode;

/// Where the shared deployment is listening. Set by the harness on each node
/// process, and inherited by this one.
const LEDGER_URL_ENV: &str = "APHELION_FAKE_LEDGER_URL";

#[derive(serde::Deserialize)]
struct CliOutput {
    stdout: String,
    stderr: String,
    error: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let Some(invoke) = parse(&args) else {
        eprintln!("fake-stellar: unsupported invocation: {args:?}");
        return ExitCode::FAILURE;
    };

    let Ok(base) = std::env::var(LEDGER_URL_ENV) else {
        eprintln!("fake-stellar: {LEDGER_URL_ENV} is not set");
        return ExitCode::FAILURE;
    };

    let response = reqwest::Client::new()
        .post(format!("{base}/invoke"))
        .json(&invoke)
        .send()
        .await;

    let out: CliOutput = match response {
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("fake-stellar: deployment returned unreadable output: {e}");
                return ExitCode::FAILURE;
            }
        },
        Err(e) => {
            eprintln!("fake-stellar: cannot reach the deployment: {e}");
            return ExitCode::FAILURE;
        }
    };

    // A contract error is a non-zero exit with the reason on stderr, which is
    // what `CliChain::run` turns into `NodeError::Chain`.
    if let Some(reason) = out.error {
        eprintln!("{reason}");
        return ExitCode::FAILURE;
    }

    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    ExitCode::SUCCESS
}

#[derive(serde::Serialize)]
struct Invoke {
    contract: String,
    func: String,
    args: HashMap<String, String>,
}

/// Pull the contract, the function and its named arguments out of a
/// `contract invoke` command line.
///
/// Everything before `--` is the CLI's own configuration, of which only
/// `--id` matters here; everything after it is the contract call. Flags the
/// node passes but this fixture has no use for (`--rpc-url`, `--send`, the
/// source account) are skipped rather than rejected, so that adding one to
/// `CliChain` does not break the harness.
fn parse(args: &[String]) -> Option<Invoke> {
    if args.first().map(String::as_str) != Some("contract")
        || args.get(1).map(String::as_str) != Some("invoke")
    {
        return None;
    }

    let split = args.iter().position(|a| a == "--")?;
    let (head, tail) = (&args[2..split], &args[split + 1..]);

    let mut contract = None;
    let mut i = 0;
    while i < head.len() {
        if head[i] == "--id" {
            contract = head.get(i + 1).cloned();
            i += 2;
        } else {
            i += 1;
        }
    }

    let func = tail.first()?.clone();
    let mut named = HashMap::new();
    let mut i = 1;
    while i < tail.len() {
        if let Some(name) = tail[i].strip_prefix("--") {
            // A flag with no value is a boolean; none are used today, but
            // treating it as empty is closer to the CLI than panicking.
            named.insert(
                name.to_string(),
                tail.get(i + 1).cloned().unwrap_or_default(),
            );
            i += 2;
        } else {
            i += 1;
        }
    }

    Some(Invoke {
        contract: contract?,
        func,
        args: named,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn reads_a_view_call_the_way_clichain_builds_it() {
        let invoke = parse(&argv(
            "contract invoke --id CAGG --source-account SECRET --rpc-url http://x \
             --network-passphrase passphrase --send no -- get_price --feed BTC_USD",
        ))
        .expect("parsed");

        assert_eq!(invoke.contract, "CAGG");
        assert_eq!(invoke.func, "get_price");
        assert_eq!(invoke.args.get("feed").unwrap(), "BTC_USD");
        // `--send no` sits before the separator and is the CLI's business,
        // not the contract's.
        assert!(!invoke.args.contains_key("send"));
    }

    #[test]
    fn reads_every_argument_of_a_submission() {
        let invoke = parse(&argv(
            "contract invoke --id CAGG --source-account SECRET --rpc-url http://x \
             --network-passphrase passphrase -- submit_price --feed BTC_USD --pubkey ab12 \
             --price 6423155000000 --timestamp 1735689600 --confidence_bps 25 --nonce 7 \
             --signature ffee",
        ))
        .expect("parsed");

        assert_eq!(invoke.func, "submit_price");
        assert_eq!(invoke.args.get("price").unwrap(), "6423155000000");
        assert_eq!(invoke.args.get("nonce").unwrap(), "7");
        assert_eq!(invoke.args.get("signature").unwrap(), "ffee");
    }

    #[test]
    fn refuses_a_command_it_does_not_understand() {
        // Better to fail loudly than to answer a call the real CLI would have
        // handled differently.
        assert!(parse(&argv("contract deploy --wasm foo.wasm")).is_none());
        assert!(parse(&argv("contract invoke --id CAGG")).is_none(), "no --");
    }
}
