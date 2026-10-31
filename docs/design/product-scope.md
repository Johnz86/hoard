# Product scope

Hoard makes existing resources discoverable and usable across machines. It indexes nominated
folders without moving or renaming their contents, exposes a dense search interface and
serves bytes through protocols clients already use.

## Supported workflows

| Workflow | Behavior | Owning reference |
| --- | --- | --- |
| Set up a node | Empty arrival guides folder selection and sharing configuration | [Getting started](../user-guide/getting-started.md) |
| Index folders | Git, Hugging Face caches and loose folders; incremental scans and watching | [Catalog](../reference/native-catalog.md) |
| Find resources | Shared model/project results, query grammar, facets and saved shelves | [Hoard API](../reference/api/hoard.md) |
| Review a project | Committed README, documentation links, file previews and refs | [UI specification](ui-spec.md) |
| Acquire resources | Git cloning, Hub-compatible downloads and peer pulls | [Hub compatibility](../reference/api/hugging-face.md) |
| Assemble model sets | Directional compatibility, named loadouts and filesystem projections | [Hoard API](../reference/api/hoard.md) |
| Manage sources | Persistent removal preserves original files; history cleanup is separate | [Getting started](../user-guide/getting-started.md) |

A Git project is a resource, not a model classification. Internal Hub namespaces do not define
its presentation. Model compatibility and loadout controls apply only where meaningful.
Search is the primary discovery interface; Sources manages what a node indexes and shares.

## Boundaries

Hoard does not train, convert, quantize or run models. It is not an account-based Git forge:
there are no issues, pull requests or repository editing workflows. There is no cloud account
or telemetry requirement. Mobile/tablet layouts and the proposed TREE/DISK views are not
implemented interfaces.

The write token protects mutations, not resource reads. A configured sharing URL does not
establish network reachability. Pre-release catalog compatibility follows the policy in
[CLAUDE.md](../../CLAUDE.md#compatibility); see [operations](../operations/deployment.md)
before upgrades or exposure to a network.
