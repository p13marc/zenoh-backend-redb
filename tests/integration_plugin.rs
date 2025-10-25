//! Integration tests for Zenoh plugin functionality.

#[cfg(feature = "plugin")]
mod plugin_tests {
    use zenoh_backend_redb::plugin::{
        DEFAULT_ROOT_DIR, NONE_KEY, PROP_STORAGE_CACHE_SIZE, PROP_STORAGE_CREATE_DB,
        PROP_STORAGE_DB_FILE, PROP_STORAGE_DIR, PROP_STORAGE_FSYNC, PROP_STORAGE_READ_ONLY,
        RedbBackendPlugin, SCOPE_ENV_VAR,
    };
    use zenoh_plugin_trait::Plugin;

    #[test]
    fn test_plugin_metadata() {
        assert_eq!(RedbBackendPlugin::DEFAULT_NAME, "redb_backend");
        assert!(!RedbBackendPlugin::PLUGIN_VERSION.is_empty());
        assert!(!RedbBackendPlugin::PLUGIN_LONG_VERSION.is_empty());
    }

    #[test]
    fn test_none_key_constant() {
        assert_eq!(NONE_KEY, "@@none_key@@");
    }

    #[test]
    fn test_plugin_constants() {
        assert_eq!(SCOPE_ENV_VAR, "ZENOH_BACKEND_REDB_ROOT");
        assert_eq!(DEFAULT_ROOT_DIR, "zenoh_backend_redb");
        assert_eq!(PROP_STORAGE_DIR, "dir");
        assert_eq!(PROP_STORAGE_DB_FILE, "db_file");
        assert_eq!(PROP_STORAGE_CREATE_DB, "create_db");
        assert_eq!(PROP_STORAGE_READ_ONLY, "read_only");
        assert_eq!(PROP_STORAGE_CACHE_SIZE, "cache_size");
        assert_eq!(PROP_STORAGE_FSYNC, "fsync");
    }

    #[test]
    fn test_plugin_feature_enabled() {
        // This test verifies that the plugin feature is properly enabled
        // and the plugin module is accessible
        use zenoh_backend_redb::plugin::RedbVolume;

        // Just ensure the types are accessible
        let _type_check: Option<RedbVolume> = None;
        // Test passes if types are accessible
    }

    #[test]
    fn test_plugin_exports() {
        // Verify plugin exports are accessible
        use zenoh_backend_redb::{RedbBackendPlugin, RedbVolume};

        // Type checks to ensure exports work
        let _plugin: Option<RedbBackendPlugin> = None;
        let _volume: Option<Box<RedbVolume>> = None;
        // Test passes if types are accessible
    }
}

#[cfg(not(feature = "plugin"))]
mod no_plugin_tests {
    #[test]
    fn test_plugin_feature_disabled() {
        // This test just verifies that the test suite compiles without plugin feature
        assert!(true);
    }
}
