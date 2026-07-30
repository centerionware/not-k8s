use super::*;

#[test]
fn matches_container_logs() {
    assert_eq!(
        parse_route("/containerLogs/default/mypod/mycontainer"),
        Route::ContainerLogs { namespace: "default".to_string(), pod: "mypod".to_string(), container: "mycontainer".to_string() }
    );
}

#[test]
fn matches_exec() {
    assert_eq!(
        parse_route("/exec/default/mypod/mycontainer"),
        Route::Exec { namespace: "default".to_string(), pod: "mypod".to_string(), container: "mycontainer".to_string() }
    );
}

#[test]
fn matches_attach() {
    assert_eq!(
        parse_route("/attach/kube-system/mypod/mycontainer"),
        Route::Attach { namespace: "kube-system".to_string(), pod: "mypod".to_string(), container: "mycontainer".to_string() }
    );
}

#[test]
fn matches_port_forward_without_a_container_segment() {
    assert_eq!(parse_route("/portForward/default/mypod"), Route::PortForward { namespace: "default".to_string(), pod: "mypod".to_string() });
}

#[test]
fn unknown_prefix_is_not_found() {
    assert_eq!(parse_route("/healthz"), Route::NotFound);
    assert_eq!(parse_route("/"), Route::NotFound);
    assert_eq!(parse_route(""), Route::NotFound);
}

#[test]
fn wrong_segment_count_is_not_found() {
    assert_eq!(parse_route("/containerLogs/default/mypod"), Route::NotFound);
    assert_eq!(parse_route("/exec/default/mypod/mycontainer/extra"), Route::NotFound);
}

#[test]
fn leading_and_trailing_slashes_are_tolerated() {
    assert_eq!(
        parse_route("/containerLogs/default/mypod/mycontainer/"),
        Route::ContainerLogs { namespace: "default".to_string(), pod: "mypod".to_string(), container: "mycontainer".to_string() }
    );
}
