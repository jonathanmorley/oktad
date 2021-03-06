#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;

mod aws;
mod config;
mod okta;
mod saml;

use crate::aws::credentials::CredentialsStore;
use crate::config::Config;
use crate::okta::client::Client as OktaClient;

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};

use failure::Error;
use glob::Pattern;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
pub struct Args {
    /// Profile to update
    #[structopt(default_value = "*", parse(try_from_str))]
    pub profiles: Pattern,

    /// Okta organization to use
    #[structopt(
        short = "o",
        long = "organizations",
        default_value = "*",
        parse(try_from_str)
    )]
    pub organizations: Pattern,

    /// Forces new credentials
    #[structopt(short = "f", long = "force-new")]
    #[cfg(not(target_os = "linux"))]
    pub force_new: bool,

    /// Sets the level of verbosity
    #[structopt(short = "v", long = "verbose", parse(from_occurrences))]
    pub verbosity: usize,

    /// Silence all output
    #[structopt(short = "q", long = "quiet")]
    pub quiet: bool,

    /// Fetch profiles asynchronously
    #[structopt(short = "a", long = "async")]
    pub asynchronous: bool,
}

#[paw::main]
#[tokio::main]
async fn main(args: Args) -> Result<(), Error> {
    debug!("Args: {:?}", args);

    // Set Log Level
    let log_level = match args.verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env::set_var("RUST_LOG", format!("{}={}", module_path!(), log_level));
    pretty_env_logger::init();

    // Fetch config from files
    let config = Config::new()?;
    debug!("Config: {:?}", config);

    // Set up a store for AWS credentials
    let credentials_store = Arc::new(Mutex::new(CredentialsStore::new()?));

    let mut organizations = config
        .into_organizations(args.organizations.clone())
        .peekable();

    if organizations.peek().is_none() {
        bail!("No organizations found called {}", args.organizations);
    }

    for organization in organizations {
        info!("Evaluating profiles in {}", organization.name);

        let okta_client = OktaClient::new(
            organization.name.clone(),
            organization.username.clone(),
            #[cfg(not(target_os = "linux"))]
            args.force_new,
        )
        .await?;

        let credentials_map = if args.asynchronous {
            organization
                .into_credentials(&okta_client, args.profiles.clone())
                .await
                .collect()
        } else {
            let profiles = organization.into_profiles(args.profiles.clone());

            let mut credentials_map = HashMap::new();
            for profile in profiles {
                let name = profile.name.clone();

                info!("Requesting tokens for {}", profile.name);

                let credentials = profile.into_credentials(&okta_client).await.unwrap();

                credentials_map.insert(name, credentials);
            }

            credentials_map
        };

        for (name, creds) in credentials_map {
            credentials_store
                .lock()
                .unwrap()
                .profiles
                .set_sts_credentials(name.clone(), creds.into())?;
        }
    }

    let mut store = credentials_store.lock().unwrap();
    store.save()
}
