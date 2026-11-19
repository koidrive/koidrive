// SPDX-License-Identifier: AGPL-3.0-or-later

import { create, toBinary } from "@bufbuild/protobuf";
import { type Commit, CommitSchema, type Op, OpSchema } from "../gen/koidrive/v1/commit_pb";
import { type Content, ContentSchema } from "../gen/koidrive/v1/model_pb";
import { sha256 } from "../crypto";

function content(value: Content | undefined): Content | undefined {
  if (!value) return undefined;
  return create(ContentSchema, {
    size: value.size,
    mtimeNs: value.mtimeNs,
    executable: value.executable,
    chunks: value.chunks.map(({ hash, size, encoding }) => ({ hash, size, encoding })),
    chunkCount: value.chunkCount,
    manifestDigest: value.manifestDigest,
    chunker: value.chunker,
  });
}

export const DIGEST_VERSIONS = [1, 2, 3, 4, 5, 6] as const;

export type DigestVersion = (typeof DIGEST_VERSIONS)[number];

export const DIGEST_VERSION: DigestVersion = 6;

export type Digests = Record<DigestVersion, Uint8Array>;

function op(each: Op, version: DigestVersion): Op {
  const projected = opBody(each, version);
  if (version < 5) return projected;
  projected.intent = each.intent;
  projected.fields = each.fields;
  if (projected.op.case === "delete" && each.op.case === "delete") {
    projected.op.value.unseen = each.op.value.unseen;
    projected.op.value.held = each.op.value.held;
    if (version >= 6) projected.op.value.trashGroup = each.op.value.trashGroup;
  }
  if (projected.op.case === "createFile" && each.op.case === "createFile") projected.op.value.pendingFor = each.op.value.pendingFor;
  return projected;
}

function opBody({ op }: Op, version: DigestVersion): Op {
  switch (op.case) {
    case "createDir": {
      const { nodeId, parentId, name } = op.value;
      const pendingFor = version >= 3 ? op.value.pendingFor : "";
      return create(OpSchema, { op: { case: op.case, value: { nodeId, parentId, name, pendingFor } } });
    }
    case "createFile": {
      const { nodeId, parentId, name } = op.value;
      return create(OpSchema, {
        op: {
          case: op.case,
          value: {
            nodeId, parentId, name, content: content(op.value.content),
            madeOver: version >= 4 ? op.value.madeOver : [],
            sourceVersionId: version >= 4 ? op.value.sourceVersionId : "",
          },
        },
      });
    }
    case "updateContent":
      return create(OpSchema, {
        op: {
          case: op.case,
          value: {
            nodeId: op.value.nodeId,
            content: content(op.value.content),
            resend: version >= 2 && op.value.resend,
            madeOver: version >= 4 ? op.value.madeOver : [],
            sourceVersionId: version >= 4 ? op.value.sourceVersionId : "",
          },
        },
      });
    case "move": {
      const { nodeId, newParentId, newName } = op.value;
      const [pendingFor, from] = version >= 3 ? [op.value.pendingFor, op.value.from] : ["", undefined];
      return create(OpSchema, { op: { case: op.case, value: { nodeId, newParentId, newName, pendingFor, from } } });
    }
    case "delete":
      return create(OpSchema, { op: { case: op.case, value: { nodeId: op.value.nodeId } } });
    case "restore": {
      const { nodeId, versionId, targetParentId, name } = op.value;
      return create(OpSchema, { op: { case: op.case, value: { nodeId, versionId, targetParentId, name } } });
    }
    default:
      return create(OpSchema);
  }
}

export async function commitDigest(commit: Commit, version: DigestVersion = DIGEST_VERSION): Promise<Uint8Array> {
  const projected = create(CommitSchema, {
    baseSeq: commit.baseSeq,
    ops: commit.ops.map((each) => op(each, version)),
    deviceSeq: commit.deviceSeq,
    requireHead: commit.requireHead,
  });
  return sha256(toBinary(CommitSchema, projected, { writeUnknownFields: false }));
}

export async function commitDigests(commit: Commit): Promise<Digests> {
  const digests = await Promise.all(DIGEST_VERSIONS.map((version) => commitDigest(commit, version)));
  return Object.fromEntries(DIGEST_VERSIONS.map((version, index) => [version, digests[index]!])) as Digests;
}

export function digestVersion(stored: number): DigestVersion {
  const known = DIGEST_VERSIONS.find((version) => version === stored);
  if (known === undefined) throw new Error(`stored digest version ${stored} is unknown to this server`);
  return known;
}
