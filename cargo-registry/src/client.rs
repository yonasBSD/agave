use {
    clap::{crate_description, crate_name, value_t, value_t_or_exit, App, Arg, ArgMatches},
    solana_clap_utils::{
        hidden_unless_forced,
        input_validators::is_url_or_moniker,
        keypair::{DefaultSigner, SignerIndex},
    },
    solana_cli::cli::{CliConfig, DEFAULT_CONFIRM_TX_TIMEOUT_SECONDS, DEFAULT_RPC_TIMEOUT_SECONDS},
    solana_cli_config::{Config, ConfigInput},
    solana_commitment_config::CommitmentConfig,
    solana_keypair::{read_keypair_file, Keypair},
    solana_rpc_client::rpc_client::RpcClient,
    solana_rpc_client_api::config::RpcSendTransactionConfig,
    std::{error, sync::Arc, time::Duration},
};

pub(crate) struct Client {
    pub rpc_client: Arc<RpcClient>,
    pub port: u16,
    pub server_url: String,
    websocket_url: String,
    commitment: CommitmentConfig,
    cli_signers: Vec<Keypair>,
    pub authority_signer_index: SignerIndex,
    send_transaction_config: RpcSendTransactionConfig,
}

impl Client {
    pub fn get_cli_config(&'_ self) -> CliConfig<'_> {
        CliConfig {
            websocket_url: self.websocket_url.clone(),
            commitment: self.commitment,
            signers: vec![&self.cli_signers[0], &self.cli_signers[1]],
            send_transaction_config: self.send_transaction_config,
            ..CliConfig::default()
        }
    }

    fn get_keypair(
        matches: &ArgMatches<'_>,
        config_path: &str,
        name: &str,
    ) -> Result<Keypair, Box<dyn error::Error>> {
        let (_, default_signer_path) = ConfigInput::compute_keypair_path_setting(
            matches.value_of(name).unwrap_or(""),
            config_path,
        );

        let default_signer = DefaultSigner::new(name, default_signer_path);

        read_keypair_file(default_signer.path)
    }

    fn get_clap_app<'ab, 'v>(name: &str, about: &'ab str, version: &'v str) -> App<'ab, 'v> {
        App::new(name)
            .about(about)
            .version(version)
            .arg(
                Arg::with_name("skip_preflight")
                    .long("skip-preflight")
                    .global(true)
                    .takes_value(false)
                    .help("Skip the preflight check when sending transactions"),
            )
            .arg(
                Arg::with_name("config_file")
                    .short("C")
                    .long("config")
                    .value_name("FILEPATH")
                    .takes_value(true)
                    .global(true)
                    .help("Configuration file to use"),
            )
            .arg(
                Arg::with_name("json_rpc_url")
                    .short("u")
                    .long("url")
                    .value_name("URL_OR_MONIKER")
                    .takes_value(true)
                    .global(true)
                    .validator(is_url_or_moniker)
                    .help(
                        "URL for Solana's JSON RPC or moniker (or their first letter): \
                       [mainnet-beta, testnet, devnet, localhost]",
                    ),
            )
            .arg(
                Arg::with_name("keypair")
                    .short("k")
                    .long("keypair")
                    .value_name("KEYPAIR")
                    .global(true)
                    .takes_value(true)
                    .help("Filepath or URL to a keypair"),
            )
            .arg(
                Arg::with_name("authority")
                    .short("a")
                    .long("authority")
                    .value_name("KEYPAIR")
                    .global(true)
                    .takes_value(true)
                    .help("Filepath or URL to program authority keypair"),
            )
            .arg(
                Arg::with_name("port")
                    .short("p")
                    .long("port")
                    .value_name("PORT")
                    .global(true)
                    .takes_value(true)
                    .help("Cargo registry's local TCP port. The server will bind to this port and wait for requests."),
            )
            .arg(
                Arg::with_name("server_url")
                    .short("s")
                    .long("server-url")
                    .value_name("URL_OR_MONIKER")
                    .takes_value(true)
                    .global(true)
                    .validator(is_url_or_moniker)
                    .help(
                        "URL where the registry service will be hosted. Default: http://0.0.0.0:<port>",
                    ),
            )
            .arg(
                Arg::with_name("commitment")
                    .long("commitment")
                    .takes_value(true)
                    .possible_values(&[
                        "processed",
                        "confirmed",
                        "finalized",
                    ])
                    .value_name("COMMITMENT_LEVEL")
                    .hide_possible_values(true)
                    .global(true)
                    .help("Return information at the selected commitment level [possible values: processed, confirmed, finalized]"),
            )
            .arg(
                Arg::with_name("rpc_timeout")
                    .long("rpc-timeout")
                    .value_name("SECONDS")
                    .takes_value(true)
                    .default_value(DEFAULT_RPC_TIMEOUT_SECONDS)
                    .global(true)
                    .hidden(hidden_unless_forced())
                    .help("Timeout value for RPC requests"),
            )
            .arg(
                Arg::with_name("confirm_transaction_initial_timeout")
                    .long("confirm-timeout")
                    .value_name("SECONDS")
                    .takes_value(true)
                    .default_value(DEFAULT_CONFIRM_TX_TIMEOUT_SECONDS)
                    .global(true)
                    .hidden(hidden_unless_forced())
                    .help("Timeout value for initial transaction status"),
            )
    }

    pub(crate) fn new() -> Result<Client, Box<dyn error::Error>> {
        let matches = Self::get_clap_app(
            crate_name!(),
            crate_description!(),
            solana_version::version!(),
        )
        .get_matches();

        let cli_config = if let Some(config_file) = matches.value_of("config_file") {
            Config::load(config_file).unwrap_or_default()
        } else {
            Config::default()
        };

        let (_, json_rpc_url) = ConfigInput::compute_json_rpc_url_setting(
            matches.value_of("json_rpc_url").unwrap_or(""),
            &cli_config.json_rpc_url,
        );

        let (_, websocket_url) = ConfigInput::compute_websocket_url_setting(
            matches.value_of("websocket_url").unwrap_or(""),
            &cli_config.websocket_url,
            matches.value_of("json_rpc_url").unwrap_or(""),
            &cli_config.json_rpc_url,
        );

        let (_, commitment) = ConfigInput::compute_commitment_config(
            matches.value_of("commitment").unwrap_or(""),
            &cli_config.commitment,
        );

        let rpc_timeout = value_t_or_exit!(matches, "rpc_timeout", u64);
        let rpc_timeout = Duration::from_secs(rpc_timeout);

        let confirm_transaction_initial_timeout =
            value_t_or_exit!(matches, "confirm_transaction_initial_timeout", u64);
        let confirm_transaction_initial_timeout =
            Duration::from_secs(confirm_transaction_initial_timeout);

        let payer_keypair = Self::get_keypair(&matches, &cli_config.keypair_path, "keypair")?;
        let authority_keypair = Self::get_keypair(&matches, &cli_config.keypair_path, "authority")?;

        let port = value_t_or_exit!(matches, "port", u16);

        let server_url =
            value_t!(matches, "server_url", String).unwrap_or(format!("http://0.0.0.0:{port}"));

        let skip_preflight = matches.is_present("skip_preflight");

        Ok(Client {
            rpc_client: Arc::new(RpcClient::new_with_timeouts_and_commitment(
                json_rpc_url.to_string(),
                rpc_timeout,
                commitment,
                confirm_transaction_initial_timeout,
            )),
            port,
            server_url,
            websocket_url,
            commitment,
            cli_signers: vec![payer_keypair, authority_keypair],
            authority_signer_index: 1,
            send_transaction_config: RpcSendTransactionConfig {
                skip_preflight,
                preflight_commitment: Some(commitment.commitment),
                ..RpcSendTransactionConfig::default()
            },
        })
    }
}
