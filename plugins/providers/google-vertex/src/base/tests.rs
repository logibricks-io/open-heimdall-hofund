use super::{
    GLOBAL_MODELS, base_url, check_credentials_file, check_settings, check_token_urls, host,
    location_of, models_for,
};

#[test]
fn the_host_follows_the_location() {
    for (location, expected) in [
        (
            "",
            "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global",
        ),
        (
            " Global ",
            "https://aiplatform.googleapis.com/v1/projects/my-proj/locations/global",
        ),
        (
            "us",
            "https://aiplatform.us.rep.googleapis.com/v1/projects/my-proj/locations/us",
        ),
        (
            "eu",
            "https://aiplatform.eu.rep.googleapis.com/v1/projects/my-proj/locations/eu",
        ),
        (
            "us-central1",
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-proj/locations/us-central1",
        ),
    ] {
        assert_eq!(
            base_url("My-Proj", location, "").as_deref(),
            Ok(expected),
            "{location:?}"
        );
    }
    assert_eq!(host("global"), "aiplatform.googleapis.com");
}

#[test]
fn an_override_replaces_the_host_and_a_pasted_url_is_cut_back() {
    let root = "http://127.0.0.1:9/v1/projects/my-proj/locations/us";
    assert_eq!(
        base_url("my-proj", "us", "http://127.0.0.1:9/").as_deref(),
        Ok(root)
    );
    assert_eq!(
        base_url(
            "my-proj",
            "us",
            "http://127.0.0.1:9/v1/projects/my-proj/locations/us/publishers/google/models/gemini-2.5-pro:generateContent"
        )
        .as_deref(),
        Ok(root)
    );
    assert_eq!(
        base_url("my-proj", "", "relay.example").as_deref(),
        Ok("https://relay.example/v1/projects/my-proj/locations/global")
    );
    assert!(base_url("my-proj", "", "http://[::1").is_err());
}

#[test]
fn project_and_location_are_checked() {
    assert_eq!(
        base_url(" ", "", ""),
        Err("Google Vertex AI needs the id of your Google Cloud project".to_owned())
    );
    for project in ["p", "-p", "p-", "a/b", "a b", "a_b"] {
        let error = base_url(project, "", "").expect_err(project);
        assert!(error.contains("is no Google Cloud project id"), "{error}");
    }
    assert!(base_url("example.com:my-project", "", "").is_ok());
    for location in ["us_central1", "1us", "us-", "us--a", "us/x"] {
        let error = base_url("my-proj", location, "").expect_err(location);
        assert!(error.contains("is no Vertex AI location"), "{error}");
    }
}

#[test]
fn the_location_is_read_back_from_the_base() {
    assert_eq!(
        location_of("https://h/v1/projects/my-proj/locations/us-central1"),
        "us-central1"
    );
    assert_eq!(location_of("https://h/v1/projects/p"), "");
    assert_eq!(location_of("not a url"), "");
}

#[test]
fn each_location_has_its_static_models() {
    assert_eq!(models_for(""), GLOBAL_MODELS);
    assert_eq!(models_for(" GLOBAL "), GLOBAL_MODELS);
    assert!(models_for("us").contains(&"gemini-3.5-flash"));
    assert!(!models_for("eu").contains(&"gemini-2.5-pro"));
    assert_eq!(
        models_for("us-central1"),
        &[
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-lite"
        ]
    );
}

#[test]
fn resolved_token_urls_require_https_or_a_loopback_host() {
    for good in [
        "",
        "https://oauth2.example/token",
        "http://localhost:8080/token",
        "http://127.0.0.1:9/token",
        "http://[::1]:9/token",
    ] {
        assert!(check_token_urls(good, good).is_ok(), "{good}");
    }
    for (token_url, iam_root, key) in [
        ("http://example.com/SECRET", "", "token_url"),
        ("", "ftp://127.0.0.1/SECRET", "iam_root"),
        ("https://user:SECRET@example.com/", "", "token_url"),
        ("not a url SECRET", "", "token_url"),
    ] {
        let error = check_token_urls(token_url, iam_root).expect_err(token_url);
        let text = error.to_string();
        assert!(text.contains(key), "{text}");
        assert!(text.contains("https URL"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }
}

#[test]
fn a_credentials_path_is_absolute_or_home_relative() {
    for good in ["", "~", "~/adc.json", "/etc/adc.json"] {
        assert!(check_settings(good, "").is_ok(), "{good}");
    }
    let error = check_settings("adc.json", "")
        .expect_err("relative")
        .to_string();
    assert!(error.contains("credentials"), "{error}");
    assert!(error.contains("must be a full path"), "{error}");
}

#[test]
fn an_impersonated_account_is_an_email() {
    assert!(check_settings("", "worker@p.iam.gserviceaccount.com").is_ok());
    for bad in ["worker", "@p", "w@", "w x@p", "w@p/q", "w@p\t"] {
        let error = check_settings("", bad).expect_err(bad).to_string();
        assert!(error.contains("impersonate"), "{error}");
        assert!(error.contains("is no service account's email"), "{error}");
    }
}

#[test]
fn config_check_wants_a_configured_file_to_exist() {
    let dir = tempfile::tempdir().expect("scratch");
    let file = dir.path().join("adc.json");
    let path = file.display().to_string();
    assert!(check_credentials_file("").is_ok());
    let error = check_credentials_file(&path)
        .expect_err("missing")
        .to_string();
    assert!(error.contains("credentials"), "{error}");
    assert!(error.contains("names no file"), "{error}");
    std::fs::write(&file, b"{}").expect("write");
    assert!(check_credentials_file(&path).is_ok());
    let error = check_credentials_file("adc.json")
        .expect_err("relative")
        .to_string();
    assert!(error.contains("must be a full path"), "{error}");
    let error = check_credentials_file(&dir.path().display().to_string())
        .expect_err("a folder")
        .to_string();
    assert!(error.contains("names no file"), "{error}");
}
