use dotenv::dotenv;
use url::Url;
use sdk::auth::Client;
use sdk::create_payment::{Payment, Secrets, User};
use sdk::TlBuilder;
use uuid::Uuid;

#[derive(serde::Deserialize, Debug)]
struct Config {
    client_id: String,
    client_secret: Uuid,
    auth_server_uri: Url,
    certificate_id: Uuid,
    private_key: String,
    environment_uri: Url,
}

impl Config {
    fn read() -> anyhow::Result<Self> {
        dotenv()?;
        let config = envy::from_env::<Self>()?;
        Ok(config)
    }
}

fn payment() -> Payment {
    Payment {
        amount_in_minor: 1,
        currency: sdk::create_payment::Currency::Gbp,
        payment_method: sdk::create_payment::PaymentMethod::BankTransfer,
        beneficiary: sdk::create_payment::Beneficiary::MerchantAccount {
            id: Uuid::new_v4(),
            name: "First Last".to_owned(),
        },
        user: User::New {
            name: "username".to_string(),
        },
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::read()?;
    let client = Client::new(config.client_id, config.client_secret);
    let secrets = Secrets::new(config.certificate_id, config.private_key);
    let mut tl = TlBuilder::new(secrets, client)
        .with_auth_server(config.auth_server_uri)
        .with_environment_uri(config.environment_uri)
        .build();
    tl.create_payment(&payment()).await?;
    Ok(())
}
