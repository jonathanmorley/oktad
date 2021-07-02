#[macro_use]
extern crate failure;
#[macro_use]
extern crate log;

mod aws;
mod config;
mod okta;
mod saml;

use crate::aws::credentials::CredentialsStore;
use crate::aws::role::Role;
use crate::config::organization::Organization;
use crate::config::profile::Profile;
use crate::config::Config;
use crate::okta::client::Client as OktaClient;

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};

use failure::Error;
use glob::Pattern;
use rusoto_sts::Credentials;
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
    pub force_new: bool,

    /// Sets the level of verbosity
    #[structopt(short = "v", long = "verbose", parse(from_occurrences))]
    pub verbosity: usize,

    /// Silence all output
    #[structopt(short = "q", long = "quiet")]
    pub quiet: bool,

    /// Run in an asynchronous manner (parallel)
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

    let mut organizations = config.organizations(args.organizations.clone()).peekable();

    if organizations.peek().is_none() {
        bail!("No organizations found called {}", args.organizations);
    }

    for organization in organizations {
        info!("Evaluating profiles in {}", organization.name);

        let okta_client = OktaClient::new(
            organization.name.clone(),
            organization.username.clone(),
            args.force_new,
        ).await?;

        // Collect here and re-iter below in case we want to be async.
        let profiles = organization
            .profiles(args.profiles.clone())
            .collect::<Vec<&Profile>>();

        if profiles.is_empty() {
            warn!(
                "No profiles found matching {} in {}",
                args.profiles, organization.name
            );
            continue;
        }

        let mut org_credentials = HashMap::new();
        for profile in profiles {
            let credentials = fetch_credentials(&okta_client, &organization, &profile).await?;
            org_credentials.insert(profile.name.clone(), credentials);
        }

        for (name, creds) in org_credentials {
            credentials_store
                .lock()
                .unwrap()
                .set_profile(name.clone(), creds)?;
        }
    }

    let store = credentials_store.lock().unwrap();
    store.save()
}

async fn fetch_credentials(
    client: &OktaClient,
    organization: &Organization,
    profile: &Profile,
) -> Result<Credentials, Error> {
    info!(
        "Requesting tokens for {}/{}",
        &organization.name, profile.name
    );

    let app_link = client
        .app_links(None).await?
        .into_iter()
        .find(|app_link| {
            app_link.app_name == "amazon_aws" && app_link.label == profile.application_name
        })
        .ok_or_else(|| {
            format_err!(
                "Could not find Okta application for profile {}/{}",
                organization.name,
                profile.name
            )
        })?;

    debug!("Application Link: {:?}", &app_link);

    let saml = client.get_saml_response(app_link.link_url).await.map_err(|e| {
        format_err!(
            "Error getting SAML response for profile {} ({})",
            profile.name,
            e
        )
    })?;

    let roles = saml.roles;

    debug!("SAML Roles: {:?}", &roles);

    let role: Role = roles
        .into_iter()
        .find(|r| r.role_name().map(|r| r == profile.role).unwrap_or(false))
        .ok_or_else(|| {
            format_err!(
                "No matching role ({}) found for profile {}",
                profile.role,
                &profile.name
            )
        })?;

    trace!(
        "Found role: {} for profile {}",
        role.role_arn,
        &profile.name
    );

    let assumption_response = aws::role::assume_role(role, saml.raw, profile.duration_seconds)
        .await
        .map_err(|e| format_err!("Error assuming role for profile {} ({})", profile.name, e))?;

    let credentials = assumption_response
        .credentials
        .ok_or_else(|| format_err!("Error fetching credentials from assumed AWS role"))?;

    trace!("Credentials: {:?}", credentials);

    Ok(credentials)
}
