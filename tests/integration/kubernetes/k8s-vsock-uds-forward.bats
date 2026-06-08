#!/usr/bin/env bats
#
# Copyright (c) 2026 Kata Contributors
#
# SPDX-License-Identifier: Apache-2.0
#

load "${BATS_TEST_DIRNAME}/lib.sh"
load "${BATS_TEST_DIRNAME}/../../common.bash"
load "${BATS_TEST_DIRNAME}/tests_common.sh"

pod_name="vsock-uds-forward-pod"
vsock_fwd_port="15001"
guest_fwd_sock="/tmp/kata-vsock-fwd.sock"
uds_dir="/var/run/kata-vsock-uds-test"
uds_path="${uds_dir}/fwd.sock"
host_socat_log="/tmp/kata-vsock-uds-host-socat.log"
expected_response="kata-vsock-uds-forward-ok"
guest_request="test"
guest_client_hold=5
vsock_uds_forward_setting="${vsock_fwd_port}:${uds_path}"
guest_unix_client_cmd=(socat - "UNIX-CONNECT:${guest_fwd_sock}")
guest_relay_wait_cmd=(test -S "${guest_fwd_sock}")

kata_runtime_config_dir() {
	local shim base rs_dir go_dir
	shim="${KATA_HYPERVISOR}"
	base="/opt/kata/share/defaults/kata-containers"
	rs_dir="${base}/runtime-rs/runtimes/${shim}"
	go_dir="${base}/runtimes/${shim}"

	if [[ "$(exec_host "${node}" "test -d '${rs_dir}' && echo yes")" == "yes" ]]; then
		echo "${rs_dir}"
		return
	fi
	if [[ "$(exec_host "${node}" "test -d '${go_dir}' && echo yes")" == "yes" ]]; then
		echo "${go_dir}"
		return
	fi
	die "no Kata runtime config dir for shim ${shim} (KATA_HYPERVISOR=${KATA_HYPERVISOR})"
}

patch_vsock_uds_forward_dropin() {
	local dir
	dir=$(kata_runtime_config_dir)

	VSOCK_UDS_DROPIN_PATH="${dir}/config.d/99-vsock-uds-forward-test.toml"
	export VSOCK_UDS_DROPIN_PATH

	exec_host "${node}" "mkdir -p '${dir}/config.d'"
	exec_host "${node}" "printf '%s\\n' '[runtime]' 'vsock_uds_forward = \"${vsock_uds_forward_setting}\"' > '${VSOCK_UDS_DROPIN_PATH}'"
}

restore_vsock_uds_forward_dropin() {
	[[ -n "${VSOCK_UDS_DROPIN_PATH:-}" ]] || return 0
	exec_host "${node}" "rm -f '${VSOCK_UDS_DROPIN_PATH}'" || true
}

start_host_uds_echo_server() {
	local i

	exec_host "${node}" "mkdir -p '${uds_dir}' && pkill -f 'socat UNIX-LISTEN:.*${uds_path}' 2>/dev/null || true; rm -f '${uds_path}' '${host_socat_log}'; nohup socat -v -v -d -d UNIX-LISTEN:'${uds_path}',fork,reuseaddr SYSTEM:'read -r req && echo ${expected_response}' </dev/null >>'${host_socat_log}' 2>&1 &"

	for i in $(seq 1 30); do
		if exec_host "${node}" "test -S '${uds_path}'"; then
			# Debugger pod creates the socket; shim must be able to connect.
			exec_host "${node}" "chmod 666 '${uds_path}'"
			return 0
		fi
		sleep 1
	done

	exec_host "${node}" "cat '${host_socat_log}' 2>/dev/null || true" >&3 || true
	die "host UDS echo server did not create ${uds_path}"
}

create_and_wait_test_pod() {
	kubectl create -f "${yaml_file}"
	k8s_wait_pod_be_ready "${pod_name}" "${wait_time}" || {
		kubectl describe pod "${pod_name}" >&3
		kubectl logs "${pod_name}" >&3 2>/dev/null || true
		return 1
	}
}

wait_for_guest_relay_sock() {
	local i

	# Dual-listen socat creates the unix socket only after the shim connects on vsock.
	for i in $(seq 1 60); do
		if kubectl exec "${pod_name}" -- test -S "${guest_fwd_sock}" 2>/dev/null; then
			return 0
		fi
		sleep 1
	done

	die "guest relay socket ${guest_fwd_sock} not ready"
}

guest_unix_request() {
	( printf '%s\n' "${guest_request}"; sleep "${guest_client_hold}" ) | \
		kubectl exec -i "${pod_name}" -- "${guest_unix_client_cmd[@]}"
}

setup() {
	[[ "${KATA_HYPERVISOR}" == qemu* ]] || skip "vsock UDS forward requires QEMU (KATA_HYPERVISOR=${KATA_HYPERVISOR})"

	setup_common || die "setup_common failed"
	get_pod_config_dir

	yaml_file="${pod_config_dir}/pod-vsock-uds-forward.yaml"

	policy_settings_dir="$(create_tmp_policy_settings_dir "${pod_config_dir}")"
	add_exec_to_policy_settings "${policy_settings_dir}" "${guest_unix_client_cmd[@]}"
	add_exec_to_policy_settings "${policy_settings_dir}" "${guest_relay_wait_cmd[@]}"
	add_requests_to_policy_settings "${policy_settings_dir}" \
		"CloseStdinRequest" "ReadStreamRequest" "WriteStreamRequest"
	auto_generate_policy "${policy_settings_dir}" "${yaml_file}"

	patch_vsock_uds_forward_dropin

	start_host_uds_echo_server
}

@test "guest unix request is forwarded to host UDS and returns response" {
	create_and_wait_test_pod

	wait_for_guest_relay_sock

	output="$(guest_unix_request)" || {
		exec_host "${node}" "cat '${host_socat_log}' 2>/dev/null || true" >&3 || true
		die "guest unix request failed"
	}

	[[ "${output}" == *"${expected_response}"* ]]
}

teardown() {
	[[ -z "${node:-}" ]] && return

	restore_vsock_uds_forward_dropin

	exec_host "${node}" "pkill -f 'socat UNIX-LISTEN:.*${uds_path}' 2>/dev/null || true"
	exec_host "${node}" "rm -f '${uds_path}' '${host_socat_log}'"

	kubectl delete -f "${yaml_file}" --ignore-not-found=true

	delete_tmp_policy_settings_dir "${policy_settings_dir}"
	teardown_common "${node}" "${node_start_time:-}"
}
