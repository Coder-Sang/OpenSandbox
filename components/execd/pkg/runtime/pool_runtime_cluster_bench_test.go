//go:build linux

// Copyright 2026 Alibaba Group Holding Ltd.
// Licensed under the Apache License, Version 2.0

package runtime

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"testing"

	"github.com/alibaba/opensandbox/execd/pkg/isolation"
)

// TestPoolRuntimeClusterBenchmark runs one isolated EFS sample per process.
// The operator supplies a unique sample name for first-use paths and repeats
// the same name for the warm sample. It is skipped outside a benchmark Pod.
func TestPoolRuntimeClusterBenchmark(t *testing.T) {
	sample := os.Getenv("OSB_POOL_BENCH_SAMPLE")
	if sample == "" {
		t.Skip("cluster benchmark only")
	}
	workers, err := strconv.Atoi(os.Getenv("OSB_POOL_BENCH_WORKERS"))
	if err != nil || workers < 1 || workers > 16 {
		t.Fatal("OSB_POOL_BENCH_WORKERS must be 1..16")
	}
	if filepath.Base(sample) != sample || sample == "." || sample == ".." {
		t.Fatal("invalid sample name")
	}
	poolPinWorkerCount = workers
	root := filepath.Join("/mnt/storage", ".opensandbox-pool-bench", sample)
	mounts := make([]PoolMountSelector, 0, 1000)
	for i := range 500 {
		project := fmt.Sprintf("project-%03d", i)
		session := fmt.Sprintf("session-%03d", i)
		projectSubPath := filepath.Join(".opensandbox-pool-bench", sample, "projects", project)
		sessionSubPath := filepath.Join(".opensandbox-pool-bench", sample, "sessions", session)
		if err := os.MkdirAll(filepath.Join(root, "projects", project), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.MkdirAll(filepath.Join(root, "sessions", session), 0o755); err != nil {
			t.Fatal(err)
		}
		mounts = append(mounts,
			PoolMountSelector{Root: "storage", SubPath: projectSubPath, Target: "/projects/" + project, Mode: "rw"},
			PoolMountSelector{Root: "storage", SubPath: sessionSubPath, Target: "/users/bench/sessions/" + session, Mode: "ro"},
		)
	}
	spec := &PoolIsolationSpec{
		Type: "bwrap",
		Roots: map[string]PoolMountRoot{
			"storage": {
				MountRoot: "/mnt/storage", Source: "/mnt/storage",
				TargetPrefixes: []string{"/projects", "/users"}, MaxMode: "rw",
			},
		},
		Mounts: mounts,
	}
	config, err := isolation.LoadConfig("/etc/execd/isolation.toml")
	if err != nil {
		t.Fatal(err)
	}
	if err := InitHardening(config); err != nil {
		t.Fatal(err)
	}
	probe := isolation.Probe(isolation.ProbeConfig{UpperRoot: config.UpperRoot, UpperMaxBytes: config.UpperMaxBytes})
	if !probe.Available {
		t.Fatalf("bwrap unavailable: %s", probe.Message)
	}
	isolator, ok := isolation.NewBwrapWithProbe(config, probe).(isolation.LifecycleIsolator)
	if !ok {
		t.Fatal("bwrap lifecycle unavailable")
	}
	manager := NewPoolRuntimeManager(isolator, probe)
	defer func() { _ = manager.Close() }()
	if err := manager.Start(spec, "pool-bench-"+sample); err != nil {
		t.Fatal(err)
	}
	t.Logf("sample=%s workers=%d mount_count=%d outcome=success", sample, workers, len(mounts))
}
