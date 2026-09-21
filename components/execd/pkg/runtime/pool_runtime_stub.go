//go:build !linux

package runtime

import (
	"errors"
	"os/exec"

	"github.com/alibaba/opensandbox/execd/pkg/isolation"
)

type PoolMountRoot struct {
	MountRoot      string   `json:"mountRoot"`
	Source         string   `json:"source"`
	TargetPrefixes []string `json:"targetPrefixes"`
	MaxMode        string   `json:"maxMode"`
}
type PoolMountSelector struct {
	Root    string `json:"root"`
	SubPath string `json:"subPath"`
	Target  string `json:"target"`
	Mode    string `json:"mode"`
}
type PoolIsolationSpec struct {
	Type   string                   `json:"type"`
	Roots  map[string]PoolMountRoot `json:"roots"`
	Mounts []PoolMountSelector      `json:"mounts"`
}
type PoolRuntimeManager struct {
	fatal chan error
}

func NewPoolRuntimeManager(_ interface{}, _ isolation.ProbeResult) *PoolRuntimeManager {
	return &PoolRuntimeManager{fatal: make(chan error)}
}
func ValidatePoolIsolation(_ *PoolIsolationSpec) error {
	return errors.New("pool bwrap runtime requires Linux")
}
func (m *PoolRuntimeManager) Start(_ *PoolIsolationSpec) error {
	return errors.New("pool bwrap runtime requires Linux")
}
func (m *PoolRuntimeManager) Fatal() <-chan error   { return m.fatal }
func (m *PoolRuntimeManager) Close() error          { return nil }
func PoolRuntimeHealthy() bool                      { return false }
func PoolRuntimeUIDMode() string                    { return "" }
func MapFilesystemPath(path string) (string, error) { return path, nil }
func wrapPoolCommand(_ *exec.Cmd, _ bool) error     { return nil }
