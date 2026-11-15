/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

mod integration;
mod seeder;

use integration::stalwart::shared as shared_stalwart;
use vandelay::jmap::account::AccountSelector;
use vandelay::jmap::http::JmapClient;
use vandelay::jmap::session::Session;
use vandelay::logging::{Level, Logger};
use vandelay::net::auth::Auth;
use vandelay::net::http::{ClientOptions, TlsPolicy};
use vandelay::net::retry::RetryPolicy;

fn admin_client() -> JmapClient {
    JmapClient::new(
        Auth::Basic {
            user: seeder::ADMIN_USER.into(),
            password: seeder::ADMIN_PASSWORD.into(),
        },
        ClientOptions {
            retry: RetryPolicy::new(5),
            tls: TlsPolicy::AcceptInvalid,
            logger: Logger::new(Level::Quiet),
        },
    )
}

#[test]
#[ignore = "requires Docker"]
fn session_discovery_and_admin_principal_resolution() {
    let stalwart = shared_stalwart();
    let fx = seeder::provision(stalwart.base_url()).expect("provision");
    let client = admin_client();

    let session =
        Session::discover(&client, &fx.base_url).expect("session discovery via .well-known");
    assert!(
        session.api_url.starts_with("https://") || session.api_url.starts_with("http://"),
        "apiUrl should be absolute: {}",
        session.api_url
    );
    let limits = session.core_limits().expect("core limits present");
    assert!(limits.max_objects_in_get >= 1);
    assert!(limits.max_concurrent_requests >= 1);
    assert!(
        !session.accounts.is_empty(),
        "authenticated admin session must enumerate accounts"
    );

    assert_eq!(fx.domain, seeder::DOMAIN);
    assert!(
        !fx.domain_id.is_empty(),
        "seeder should have ensured a domain id"
    );
    assert_eq!(
        fx.admin_login,
        (
            seeder::ADMIN_USER.to_owned(),
            seeder::ADMIN_PASSWORD.to_owned()
        )
    );

    let target = fx.account("test1").expect("test1 seeded");
    assert!(
        !target.admin_role,
        "test1 must be a regular user, not admin"
    );
    let seeded = target.seeded.as_ref().expect("test1 seed stats");
    assert!(seeded.emails > 0, "test1 should be seeded with emails");
    assert!(
        seeded.mailboxes_created >= 7,
        "test1 layout has at least 7 mailboxes"
    );
    assert!(
        seeded.file_nodes >= 9,
        "test1 layout has at least 9 file nodes"
    );
    assert!(
        seeded.contacts > 0,
        "test1 should be seeded with at least one contact"
    );
    assert!(
        seeded.events > 0,
        "test1 should be seeded with at least one event"
    );
    assert!(
        seeded.address_books > 0,
        "test1 layout requests an extra address book"
    );
    assert!(
        seeded.calendars > 0,
        "test1 layout requests an extra calendar"
    );
    assert!(seeded.identity, "test1 layout requests a custom identity");
    assert_eq!(
        seeded.sieve_active,
        Some(true),
        "test1 layout activates a sieve script"
    );

    let resolved = AccountSelector::Name(target.email.clone())
        .resolve(&session, &client)
        .expect("admin principal resolution");
    assert_eq!(
        resolved, target.account_id,
        "{} must resolve to the seeded account id {}",
        target.email, target.account_id
    );

    seeder::teardown(stalwart.base_url()).expect("teardown");
}
