const DEPLOYMENT: &str = include_str!("../../../docs/dbx-gateway/deployment-manual.md");
const EDGE: &str = include_str!("../../../docs/dbx-gateway/edge-gateway.md");
const EDGE_CERTIFICATE: &str = include_str!("../../../docs/dbx-gateway/edge-certificate.md");
const CLIENT_CERTIFICATE: &str = include_str!("../../../docs/dbx-gateway/client-certificate.md");
const MAIN: &str = include_str!("../../../docs/dbx-gateway/main-gateway.md");
const PKI: &str = include_str!("../../../docs/dbx-gateway/pki.md");
const OPERATIONS: &str = include_str!("../../../docs/dbx-gateway/operations.md");
const CONFIGURATION: &str = include_str!("../../../docs/dbx-gateway/configuration.md");
const PACKAGE_VERIFIER: &str = include_str!("../../../scripts/verify-gateway-package.sh");
const MAIN_SYSTEMD: &str = include_str!("../../../examples/dbx-gateway/systemd/dbx-gateway-main.service");
const PKI_CLI: &str = include_str!("../src/bin/dbx-gateway-pki.rs");

#[test]
fn edge_revocation_opens_the_online_edge_only_store() {
    assert!(PKI_CLI.contains("CertificateRole::Edge => PkiStore::open_online_edge(data_dir)"));
}

#[test]
fn gateway_documentation_uses_one_service_account_model() {
    let main_user =
        "useradd --system --gid dbx-gateway --home-dir /var/lib/dbx-gateway --shell /usr/sbin/nologin dbx-gateway";
    let pki_user = "useradd --system --gid dbx-gateway --home-dir /var/lib/dbx-gateway-pki --shell /usr/sbin/nologin dbx-gateway-pki";
    assert!(DEPLOYMENT.contains(main_user));
    assert!(PKI.contains(main_user));
    assert!(DEPLOYMENT.contains(pki_user));
    assert!(PKI.contains(pki_user));
    assert!(PKI.find(main_user).unwrap() < PKI.find(pki_user).unwrap());
    assert!(PKI.find("若组或用户已由 Main 安装步骤创建").unwrap() < PKI.find("groupadd --system dbx-gateway").unwrap());
    assert!(!PKI.contains("-g dbx-gateway-pki"));
}

#[test]
fn gateway_documentation_installs_restricted_configs_for_the_runtime_group() {
    let pki_config = "install -o root -g dbx-gateway -m 0640 examples/pki.toml /etc/dbx-gateway-pki/pki.toml";
    let main_config = "install -o root -g dbx-gateway -m 0640 examples/main.toml /etc/dbx-gateway/main.toml";
    assert!(DEPLOYMENT.contains(pki_config));
    assert!(DEPLOYMENT.contains(main_config));
    assert!(PKI.contains(pki_config));
    assert!(MAIN.contains(main_config));
}

#[test]
fn online_pki_mutations_run_as_the_pki_service_user() {
    for document in [DEPLOYMENT, EDGE] {
        assert!(document.contains("sudo -u dbx-gateway-pki dbx-gateway-pki enrollment create"));
    }
    for document in [DEPLOYMENT, PKI] {
        assert!(document.contains("sudo -u dbx-gateway-pki dbx-gateway-pki edge revoke"));
    }
    for document in [DEPLOYMENT, EDGE, PKI] {
        for line in document.lines() {
            assert!(!line.starts_with("dbx-gateway-pki enrollment "), "online PKI command lacks sudo: {line}");
            assert!(!line.starts_with("dbx-gateway-pki edge revoke"), "online PKI command lacks sudo: {line}");
        }
    }
    assert!(OPERATIONS.contains("sudo -u dbx-gateway-pki openssl crl"));
    assert!(OPERATIONS.contains("sudo -u dbx-gateway-pki sqlite3"));
}

#[test]
fn primary_deployment_flow_is_explicitly_same_host_unix_socket_only() {
    assert!(DEPLOYMENT.contains("本手册主流程要求 Main 与在线 PKI 部署在同一台主机"));
    assert!(DEPLOYMENT.contains("远程 RA mTLS 不属于本手册的可直接执行流程"));
    assert!(MAIN.contains("本页的可执行步骤只覆盖同机 Unix Socket"));
    assert!(PKI.contains("本页的可执行步骤只覆盖同机 Unix Socket"));
    assert!(CONFIGURATION.contains("远程 RA mTLS 字段仅作为高级配置参考"));
    assert!(!DEPLOYMENT.contains("可与在线 PKI 同机"));
    assert!(!MAIN.contains("分机部署"));
    assert!(!PKI.contains("远程部署必须"));
}

#[test]
fn upgrade_examples_use_one_explicit_version_variable() {
    assert!(DEPLOYMENT.contains("VERSION=0.5.83"));
    assert!(MAIN.contains("VERSION=0.5.83"));
    assert!(MAIN.contains("DBX_Gateway_${VERSION}_x64.tar.gz"));
    assert!(MAIN.contains("预期 checksum 成功、程序报告版本为 `${VERSION}`"));
    assert!(!MAIN.contains("0.5.75"));
}

#[test]
fn package_verification_requires_the_complete_deployment_manual() {
    assert!(PACKAGE_VERIFIER.contains("docs/dbx-gateway/deployment-manual.md"));
    assert!(PACKAGE_VERIFIER.contains("docs/dbx-gateway/edge-certificate.md"));
    assert!(PACKAGE_VERIFIER.contains("docs/dbx-gateway/client-certificate.md"));
    assert!(PACKAGE_VERIFIER.contains("docs/dbx-gateway/deployment-manual.md \"## 2. 部署准备\""));
}

#[test]
fn custom_pki_state_file_is_documented_for_enrollment_commands() {
    assert!(PKI_CLI.contains("state_file: Option<PathBuf>"));
    assert!(PKI_CLI.contains("enrollment_state_file(&args.data_dir, args.state_file.as_deref())"));
    assert!(CONFIGURATION.contains("`enrollment create/revoke` 使用自定义 `state_file` 时必须同时传入 `--state-file`"));
    assert!(CONFIGURATION.contains("ReadWritePaths=/srv/dbx-gateway-pki"));
    assert!(CONFIGURATION.contains("systemctl daemon-reload"));
    assert!(PKI.contains("`state_file` 位于 `/var/lib/dbx-gateway-pki` 之外"));
    assert!(PKI.contains("必须把该文件单独加入同一时间点的备份"));
}

#[test]
fn edge_revocation_documentation_requires_main_blocklist_propagation() {
    assert!(PKI_CLI.contains("state.revoke_issued_certificate(&args.serial, &args.reason)"));
    assert!(
        PKI_CLI.contains("CRL was updated but online revocation state was not; retry the exact edge revoke command")
    );
    for document in [DEPLOYMENT, PKI] {
        assert!(document.contains("Main 当前不会自动读取 `edge/crl.pem`"));
        assert!(document.contains("revoked_edge_serials = [\"REPLACE_WITH_NORMALIZED_EDGE_SERIAL\"]"));
        assert!(document.contains("sudo -u dbx-gateway dbx-gateway --config /etc/dbx-gateway/main.toml check-config"));
        assert!(document.contains("systemctl kill -s HUP dbx-gateway-main.service"));
        assert!(document.contains("--state-file /var/lib/dbx-gateway-pki/gateway-state.sqlite3"));
        assert!(document.contains("必须用完全相同的参数重试，直到 SQLite 更新成功"));
    }
    for document in [EDGE, EDGE_CERTIFICATE] {
        assert!(document.contains("`--replace` 不会自动更新 Main 的 `revoked_edge_serials`"));
    }
}

#[test]
fn client_revocation_documentation_matches_main_capabilities() {
    for document in [DEPLOYMENT, CLIENT_CERTIFICATE, PKI, OPERATIONS] {
        assert!(document.contains("Main 当前不读取 Client CRL，也没有 Client serial 吊销列表"));
    }
    assert!(CLIENT_CERTIFICATE.contains("为替代证书使用新的 Client identity"));
    assert!(CLIENT_CERTIFICATE.contains("systemctl restart dbx-gateway-main.service"));
    assert!(!PKI.contains("新 identity 或新证书"));
}

#[test]
fn default_main_service_can_bind_the_documented_https_port() {
    assert!(MAIN_SYSTEMD.contains("AmbientCapabilities=CAP_NET_BIND_SERVICE"));
    assert!(MAIN_SYSTEMD.contains("CapabilityBoundingSet=CAP_NET_BIND_SERVICE"));
    assert!(PACKAGE_VERIFIER.contains("grep -q '^AmbientCapabilities=CAP_NET_BIND_SERVICE$'"));
    assert!(PACKAGE_VERIFIER.contains("grep -q '^CapabilityBoundingSet=CAP_NET_BIND_SERVICE$'"));
}
