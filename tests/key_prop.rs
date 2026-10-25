use home_camera::key::sanitize_camera_id;
use proptest::prelude::*;

proptest! {
    #[test]
    fn camera_id_is_s3_key_safe(input in ".{0,32}") {
        let out = sanitize_camera_id(&input);
        prop_assert!(!out.is_empty());
        prop_assert!(!out.contains('/'));
        prop_assert!(out.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
