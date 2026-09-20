#
# Copyright 2026 The OpenSandbox Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#

from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any, TypeVar

from attrs import define as _attrs_define

from ..models.sandbox_isolation_type import SandboxIsolationType

if TYPE_CHECKING:
    from ..models.sandbox_isolation_mount import SandboxIsolationMount


T = TypeVar("T", bound="SandboxIsolation")


@_attrs_define
class SandboxIsolation:
    """
    Attributes:
        type_ (SandboxIsolationType):
        mounts (list[SandboxIsolationMount]):
    """

    type_: SandboxIsolationType
    mounts: list[SandboxIsolationMount]

    def to_dict(self) -> dict[str, Any]:
        type_ = self.type_.value

        mounts = []
        for mounts_item_data in self.mounts:
            mounts_item = mounts_item_data.to_dict()
            mounts.append(mounts_item)

        field_dict: dict[str, Any] = {}

        field_dict.update(
            {
                "type": type_,
                "mounts": mounts,
            }
        )

        return field_dict

    @classmethod
    def from_dict(cls: type[T], src_dict: Mapping[str, Any]) -> T:
        from ..models.sandbox_isolation_mount import SandboxIsolationMount

        d = dict(src_dict)
        type_ = SandboxIsolationType(d.pop("type"))

        mounts = []
        _mounts = d.pop("mounts")
        for mounts_item_data in _mounts:
            mounts_item = SandboxIsolationMount.from_dict(mounts_item_data)

            mounts.append(mounts_item)

        sandbox_isolation = cls(
            type_=type_,
            mounts=mounts,
        )

        return sandbox_isolation
