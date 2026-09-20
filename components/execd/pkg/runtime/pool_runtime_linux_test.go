//go:build linux

// Copyright 2026 Alibaba Group Holding Ltd.
// Licensed under the Apache License, Version 2.0

package runtime

import (
	"testing"

	"github.com/stretchr/testify/require"
)

func validPoolIsolationSpec() *PoolIsolationSpec {
	return &PoolIsolationSpec{
		Type: "bwrap",
		Roots: map[string]PoolMountRoot{
			"projects": {
				MountRoot:      "/storage",
				Source:         "/storage/projects",
				TargetPrefixes: []string{"/workspace"},
				MaxMode:        "rw",
			},
		},
		Mounts: []PoolMountSelector{{
			Root: "projects", SubPath: "project-A", Target: "/workspace/a", Mode: "rw",
		}},
	}
}

func TestValidatePoolIsolation(t *testing.T) {
	require.NoError(t, ValidatePoolIsolation(validPoolIsolationSpec()))
}

func TestValidatePoolIsolationRejectsTraversal(t *testing.T) {
	spec := validPoolIsolationSpec()
	spec.Mounts[0].SubPath = "../sibling"
	require.ErrorContains(t, ValidatePoolIsolation(spec), "subPath")
}

func TestValidatePoolIsolationRejectsPermissionEscalation(t *testing.T) {
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MaxMode = "ro"
	spec.Roots["projects"] = root
	require.ErrorContains(t, ValidatePoolIsolation(spec), "maxMode")
}

func TestValidatePoolIsolationRejectsOverlappingTargets(t *testing.T) {
	spec := validPoolIsolationSpec()
	spec.Mounts = append(spec.Mounts, PoolMountSelector{
		Root: "projects", SubPath: "project-B", Target: "/workspace/a/nested", Mode: "ro",
	})
	require.ErrorContains(t, ValidatePoolIsolation(spec), "overlap")
}

func TestValidatePoolIsolationRejectsControlTarget(t *testing.T) {
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.TargetPrefixes = []string{"/opt"}
	spec.Roots["projects"] = root
	spec.Mounts[0].Target = "/opt/opensandbox"
	require.ErrorContains(t, ValidatePoolIsolation(spec), "control path")
}

func TestValidatePoolIsolationRejectsControlMountRoot(t *testing.T) {
	spec := validPoolIsolationSpec()
	root := spec.Roots["projects"]
	root.MountRoot = "/opt"
	root.Source = "/opt/projects"
	spec.Roots["projects"] = root
	require.ErrorContains(t, ValidatePoolIsolation(spec), "mountRoot overlaps control path")
}
