//! `SiGitCode` served in-process, the way an app hosts it, against the testkit's mock endpoint:
//! the method is advertised, a signed-out session is refused, a stored session reaches the
//! endpoint as the bearer, and signing out takes it away again.
//!
//! One test function: the sign-in reads process-wide environment variables.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, LogoutRequest, NewSessionRequest, PromptRequest, StopReason,
    TextContent,
};
use agent_client_protocol::{Agent, Channel, Client, ConnectionTo};
use ed_acp::ServeOptions;
use ed_acp_testkit::{MockStats, Script, Step, start_mock_llm};
use sigit_agent::{METHOD_ID, SiGitCode, credentials};

#[tokio::test]
async fn the_profile_signs_in_through_its_provider_and_talks_to_the_cloud() {
    let dir = std::env::temp_dir().join(format!("sigit_agent_embedded_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let stats = Arc::new(MockStats::default());
    let url = start_mock_llm(
        Script(vec![Step::Final("hello from the cloud")]),
        stats.clone(),
    )
    .await;
    // SAFETY: this is the only test in the binary, so nothing else reads the environment.
    unsafe {
        std::env::set_var("SIGIT_CONFIG_DIR", dir.join("config"));
        std::env::set_var("SIGIT_AGENT_DATA_DIR", dir.join("data"));
        std::env::set_var("SIGIT_CLOUD_URL", &url);
        std::env::set_var("SIGIT_AGENT_MODEL", ed_acp_testkit::MODEL);
        // The mock is the only server in play.
        std::env::set_var("SIGIT_MCP_OFFICIAL", "off");
    }
    assert!(credentials::load().is_none());

    let (agent_end, client_end) = Channel::duplex();
    let opts = ServeOptions {
        yolo: false,
        surface: "app",
    };
    tokio::spawn(ed_acp::serve(
        Arc::new(SiGitCode::without_shell()),
        opts,
        agent_end,
    ));
    let cwd = dir.clone();
    let seen = stats.clone();

    Client
        .builder()
        .connect_with(client_end, |conn: ConnectionTo<Agent>| async move {
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(init.auth_methods.len(), 1);
            assert_eq!(&*init.auth_methods[0].id().0, METHOD_ID);
            assert_eq!(init.agent_info.unwrap().name, "sigit-agent");

            let refused = conn
                .send_request(NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await;
            assert!(refused.is_err(), "signed out, so no session");

            // What a finished browser sign-in leaves behind.
            credentials::store(&credentials::Credentials {
                // The mock accepts one bearer.
                access_token: ed_acp_testkit::TEST_KEY.into(),
                email: Some("dev@sigit.si".into()),
            })
            .unwrap();

            let session = conn
                .send_request(NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await?;
            let reply = conn
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("hi"))],
                ))
                .block_task()
                .await?;
            assert_eq!(reply.stop_reason, StopReason::EndTurn);
            assert!(*seen.saw_bearer.lock().unwrap());
            assert_eq!(seen.requests.load(Ordering::SeqCst), 1);

            // The model saw siGit Code's prompt and no run_command.
            let payload = seen.payloads.lock().unwrap()[0].clone();
            let system = payload["messages"][0]["content"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(system.contains("siGit"));
            assert!(system.contains("cannot run commands"));
            let tools = payload["tools"].to_string();
            assert!(tools.contains("edit_file"));
            assert!(!tools.contains("run_command"));

            conn.send_request(LogoutRequest::new()).block_task().await?;
            assert!(credentials::load().is_none());
            let after = conn
                .send_request(NewSessionRequest::new(cwd.clone()))
                .block_task()
                .await;
            assert!(after.is_err(), "signed out again");
            Ok(())
        })
        .await
        .expect("conversation");

    let _ = std::fs::remove_dir_all(&dir);
}
