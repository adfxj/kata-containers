#!/usr/bin/env bats
#
# Copyright (c) 2018 Intel Corporation
#
# SPDX-License-Identifier: Apache-2.0
#

load "${BATS_TEST_DIRNAME}/lib.sh"
load "${BATS_TEST_DIRNAME}/../../common.bash"
load "${BATS_TEST_DIRNAME}/../../hypervisor_helpers.sh"
load "${BATS_TEST_DIRNAME}/tests_common.sh"

setup() {
	is_hotplug_supported "${KATA_HYPERVISOR}" || skip "${KATA_HYPERVISOR} doesn't support memory / CPU hotplug"

	pod_name="constraints-cpu-test"
	container_name="first-cpu-container"

	weightsyspath="/sys/fs/cgroup/cpu.weight"
	maxsyspath="/sys/fs/cgroup/cpu.max"

	total_cpus=2
	# https://github.com/containers/crun/blob/main/crun.1.md#cgroup-v2
	# The weight is calculated as:
	# weight = (1 + ((shares - 2) * 9999) / 262142)
	# Kubelet maps a 500m CPU request to 512 CPU shares.
	# Integer division gives 1 + floor((512 - 2) * 9999 / 262142) = 20.
	cpu_weight_expected=20
	cpu_limit_millis_expected=500

	setup_common || die "setup_common failed"

	yaml_file="${pod_config_dir}/pod-cpu.yaml"

	# Add policy to the yaml file
	policy_settings_dir="$(create_tmp_policy_settings_dir "${pod_config_dir}")"

	num_cpus_cmd="grep -e '^processor' /proc/cpuinfo |wc -l"
	exec_num_cpus_cmd=(sh -c "${num_cpus_cmd}")
	add_exec_to_policy_settings "${policy_settings_dir}" "${exec_num_cpus_cmd[@]}"

	maxsyspath_cmd="cat ${maxsyspath}"
	exec_maxsyspath_cmd=(sh -c "${maxsyspath_cmd}")
	add_exec_to_policy_settings "${policy_settings_dir}" "${exec_maxsyspath_cmd[@]}"

	weightsyspath_cmd="cat ${weightsyspath}"
	exec_weightsyspath_cmd=(sh -c "${weightsyspath_cmd}")
	add_exec_to_policy_settings "${policy_settings_dir}" "${exec_weightsyspath_cmd[@]}"

	add_requests_to_policy_settings "${policy_settings_dir}" "ReadStreamRequest"
	auto_generate_policy "${policy_settings_dir}" "${yaml_file}"
}

@test "Check CPU constraints" {
	# Create the pod
	kubectl create -f "${yaml_file}"

	# Check pod creation
	kubectl wait --for=condition=Ready --timeout=$timeout pod "$pod_name"

	retries="10"

	# Check the total of cpus
	for _ in $(seq 1 "$retries"); do
		# Get number of cpus
		# Retry "kubectl exec" several times in case it unexpectedly returns an empty output string,
		# in an attempt to work around issues similar to https://github.com/kubernetes/kubernetes/issues/124571.
		for _ in {1..10}; do
			total_cpus_container=$(kubectl exec pod/"$pod_name" -c "$container_name" \
			-- "${exec_num_cpus_cmd[@]}")
			if [[ -n "${total_cpus_container}" ]]; then
				break
			fi
			warn "Empty output from kubectl exec" >&2
			sleep 1
		done

		# Verify number of cpus
		[ "$total_cpus_container" -le "$total_cpus" ]
		[ "$total_cpus_container" -eq "$total_cpus" ] && break
		sleep 1
	done
	[ "$total_cpus_container" -eq "$total_cpus" ]

	# Check the CPU weight derived from the request.
	for _ in {1..10}; do
		cpu_weight_container=$(kubectl exec $pod_name -c $container_name \
			-- "${exec_weightsyspath_cmd[@]}")
		if [[ -n "${cpu_weight_container}" ]]; then
			break
		fi
		warn "Empty output from kubectl exec" >&2
		sleep 1
	done
	info "cpu_weight_container = $cpu_weight_container"

	[ "$cpu_weight_container" -eq "$cpu_weight_expected" ]

	# Check the cpus inside the container
	for _ in {1..10}; do
		maxsyspath=$(kubectl exec $pod_name -c $container_name -- "${exec_maxsyspath_cmd[@]}")
		if [[ -n "${maxsyspath}" ]]; then
			break
		fi
		warn "Empty output from kubectl exec" >&2
		sleep 1
	done
	read total_cpu_quota total_cpu_period <<< ${maxsyspath}

	# A 500m limit is commonly 50000/100000. Compare in millicpus because
	# integer division of quota / period would round this down to 0.
	cpu_limit_millis_observed=$((total_cpu_quota * 1000 / total_cpu_period))

	[ "$cpu_limit_millis_observed" -eq "$cpu_limit_millis_expected" ]
}

teardown() {
	is_hotplug_supported "${KATA_HYPERVISOR}" || skip "${KATA_HYPERVISOR} doesn't support memory / CPU hotplug"

	# Debugging information
	kubectl describe "pod/$pod_name"

	kubectl delete pod "$pod_name"
	delete_tmp_policy_settings_dir "${policy_settings_dir}"
	teardown_common "${node}" "${node_start_time:-}"
}
