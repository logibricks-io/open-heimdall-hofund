use super::{check_region, header_safe, runtime_root, signing_region};

#[test]
fn a_pasted_chat_or_messages_url_becomes_the_runtime_root() {
    for url in [
        "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1/",
        "https://bedrock-runtime.eu-west-1.amazonaws.com/anthropic",
        " https://bedrock-runtime.eu-west-1.amazonaws.com// ",
    ] {
        assert_eq!(
            runtime_root(url),
            "https://bedrock-runtime.eu-west-1.amazonaws.com",
            "{url}"
        );
    }
    assert_eq!(
        runtime_root("http://proxy.test/aws"),
        "http://proxy.test/aws"
    );
    assert_eq!(runtime_root(""), "");
}

#[test]
fn the_region_option_wins_over_the_host() {
    assert_eq!(
        signing_region(
            " EU-West-2 ",
            "https://bedrock-runtime.eu-west-1.amazonaws.com"
        )
        .expect("explicit"),
        "eu-west-2"
    );
    assert_eq!(
        signing_region("", "https://Bedrock-Runtime.eu-west-1.amazonaws.com:443").expect("host"),
        "eu-west-1"
    );
    for base in [
        "https://proxy.example.test",
        "https://bedrock-runtime..amazonaws.com",
        "https://bedrock-runtime.eu-west-1.amazonaws.com.evil.test",
        "not a url",
    ] {
        let error = signing_region("", base).expect_err(base);
        assert!(
            error.to_string().contains("Converse needs a region"),
            "{base}: {error}"
        );
    }
    let error = signing_region("", "https://bedrock-runtime.us_east.amazonaws.com")
        .expect_err("bad host region");
    assert!(
        error.to_string().contains("`us_east` is not a region id"),
        "{error}"
    );
}

#[test]
fn a_region_is_short_lowercase_ascii() {
    for region in ["us-east-1", "a", "0", &"a".repeat(32)] {
        check_region(region).expect(region);
    }
    for region in ["", "US", "us east", "us/east", "us.east", &"a".repeat(33)] {
        check_region(region).expect_err(region);
    }
}

#[test]
fn header_text_has_no_space_or_control() {
    header_safe("AKIA/+=", "key").expect("plain");
    assert!(
        header_safe("", "key")
            .expect_err("empty")
            .to_string()
            .contains("Converse key is empty")
    );
    for bad in ["a b", "a\tb", "a\u{7f}b", "a\rb"] {
        assert!(
            header_safe(bad, "key")
                .expect_err(bad)
                .to_string()
                .contains("Converse key is not valid header text"),
            "{bad:?}"
        );
    }
}
