//! Loopback SSH fixture with password and two-round keyboard-interactive users.
use russh::keys::{PrivateKey, ssh_key::private::RsaKeypair};
use russh::server::{self, Auth, Response, Server as _};
use russh::{MethodKind, MethodSet};
use std::borrow::Cow;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct Authentication {
    round: u8,
}

fn reject(user: &str) -> Auth {
    let mut methods = MethodSet::empty();
    methods.push(if user == "interactive" {
        MethodKind::KeyboardInteractive
    } else {
        MethodKind::Password
    });
    Auth::Reject {
        proceed_with_methods: Some(methods),
        partial_success: false,
    }
}

fn challenge(
    name: &'static str,
    instructions: &'static str,
    prompts: &[(&'static str, bool)],
) -> Auth {
    Auth::Partial {
        name: name.into(),
        instructions: instructions.into(),
        prompts: Cow::Owned(
            prompts
                .iter()
                .map(|(prompt, echo)| (Cow::Borrowed(*prompt), *echo))
                .collect(),
        ),
    }
}

impl server::Server for Authentication {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        Self::default()
    }
}

impl server::Handler for Authentication {
    type Error = russh::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        Ok(reject(user))
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "password" && password == "test password" {
            Auth::Accept
        } else {
            reject(user)
        })
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _: &str,
        response: Option<Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        if user != "interactive" {
            return Ok(reject(user));
        }
        let Some(response) = response else {
            self.round = 0;
            return Ok(challenge(
                "Server sign-in",
                "Enter the account and password.",
                &[("Account", true), ("Password", false)],
            ));
        };
        let responses: Vec<_> = response.map(|bytes| bytes.to_vec()).collect();
        Ok(match (self.round, responses.as_slice()) {
            (0, [account, password])
                if account == b"test account" && password == b"test password" =>
            {
                self.round = 1;
                challenge("Verification", "Enter the code.", &[("Code", false)])
            }
            (1, [code]) if code == b"123456" => Auth::Accept,
            _ => reject(user),
        })
    }
}

#[tokio::main]
async fn main() {
    let key = PrivateKey::new(
        RsaKeypair::random(&mut rand::rng(), 2048).unwrap().into(),
        "",
    )
    .unwrap();
    let config = Arc::new(server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    println!(
        "{}",
        serde_json::json!({"port": socket.local_addr().unwrap().port()})
    );
    std::io::stdout().flush().unwrap();
    Authentication::default()
        .run_on_socket(config, &socket)
        .await
        .unwrap();
}
