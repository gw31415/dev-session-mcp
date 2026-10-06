mod common;
use anyhow::{Result, ensure};
use common::Fixture;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_and_discovery_real_processes() -> Result<()> {
    let fixture = Fixture::new().await?;
    for subjects in [
        json!([]),
        json!(["fixture-owner", "other"]),
        json!(["fixture-owner", "fixture-owner"]),
        json!([""]),
    ] {
        let mut process = fixture.start(subjects).await?;
        let stderr = process.rejected().await?;
        ensure!(
            stderr.contains("allowed_subjects must contain exactly one non-empty subject"),
            "wrong startup rejection"
        );
        ensure!(
            fixture.requests().is_empty(),
            "invalid owner configuration performed discovery"
        );
    }
    println!("PASS Rust startup rejects empty/multiple/duplicate/blank owners before discovery");
    for variant in ["oauth", "oidc-inserted", "oidc-appended", "oidc-trailing"] {
        let expected = fixture.discovery(variant);
        let mut process = fixture.start(json!(["fixture-owner"])).await?;
        fixture.ready(&mut process).await?;
        let count = if variant == "oauth" {
            1
        } else if variant == "oidc-appended" {
            3
        } else {
            2
        };
        let requests = fixture.requests();
        ensure!(
            requests[..count] == expected[..count] && requests[count] == "/jwks",
            "wrong discovery priority"
        );
        let access = fixture.token(json!({}))?;
        ensure!(
            fixture.modern(&access, "tools/list", json!({})).await?["tools"]
                .as_array()
                .unwrap()
                .len()
                == 20,
            "wrong tool count"
        );
        let other = fixture.token(json!({"sub":"other"}))?;
        ensure!(
            fixture
                .http
                .get(&fixture.resource)
                .bearer_auth(other)
                .send()
                .await?
                .status()
                .as_u16()
                == 403,
            "another owner accepted"
        );
        ensure!(
            fixture
                .http
                .get(&fixture.resource)
                .bearer_auth("oauth:fixture-opaque")
                .header("Cf-Access-Jwt-Assertion", &access)
                .send()
                .await?
                .status()
                .as_u16()
                == 401,
            "opaque bearer accepted"
        );
        ensure!(
            fixture
                .http
                .get(&fixture.resource)
                .header("Cf-Access-Jwt-Assertion", &access)
                .send()
                .await?
                .status()
                .as_u16()
                == 401,
            "proxy assertion accepted"
        );
        process.stop().await?;
        ensure!(!process.stderr()?.contains(&access), "bearer logged");
        println!(
            "PASS Rust {variant} discovery, owner HTTP tools and foreign/opaque/header rejection"
        );
    }
    Ok(())
}
