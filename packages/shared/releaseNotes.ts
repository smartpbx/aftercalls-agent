export type ReleaseNotesEntry = {
  version: string;
  headline: string;
  changes: string[];
  footer?: string;
};

export type ReleaseNotesSelection =
  | { kind: "show"; entries: ReleaseNotesEntry[] }
  | { kind: "none" }
  | { kind: "integrity_error"; reason: string };

type ParsedVersion = readonly [number, number, number];

const VERSION_PATTERN = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

function parseVersion(version: string): ParsedVersion | null {
  const match = VERSION_PATTERN.exec(version);
  if (!match) return null;

  const parsed = [
    Number.parseInt(match[1], 10),
    Number.parseInt(match[2], 10),
    Number.parseInt(match[3], 10),
  ] as const;
  return parsed.every(Number.isSafeInteger) ? parsed : null;
}

function compareVersions(a: ParsedVersion, b: ParsedVersion): number {
  for (let index = 0; index < a.length; index += 1) {
    if (a[index] !== b[index]) return a[index] > b[index] ? 1 : -1;
  }
  return 0;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseEntry(
  version: string,
  value: unknown,
): ReleaseNotesEntry | null {
  if (!isObject(value)) return null;
  if (typeof value.headline !== "string" || value.headline.trim() === "") {
    return null;
  }
  if (
    !Array.isArray(value.changes) ||
    value.changes.length === 0 ||
    !value.changes.every(
      (change) => typeof change === "string" && change.trim() !== "",
    )
  ) {
    return null;
  }
  if (
    value.footer !== undefined &&
    (typeof value.footer !== "string" || value.footer.trim() === "")
  ) {
    return null;
  }

  return {
    version,
    headline: value.headline,
    changes: [...value.changes],
    ...(value.footer === undefined ? {} : { footer: value.footer }),
  };
}

/**
 * Validates the entire generated artifact before selecting the versions that
 * should be presented. This function is intentionally side-effect free:
 * callers must only persist a bookmark after the user dismisses `show`.
 */
export function selectReleaseNotes(
  artifact: unknown,
  currentVersion: string,
  lastSeenVersion: string | null,
  firstInstallCap = 3,
): ReleaseNotesSelection {
  const current = parseVersion(currentVersion);
  if (!current) {
    return {
      kind: "integrity_error",
      reason: `invalid running version: ${currentVersion}`,
    };
  }
  if (!isObject(artifact)) {
    return {
      kind: "integrity_error",
      reason: "release-notes artifact is not an object",
    };
  }

  const parsedEntries = new Map<
    string,
    { version: ParsedVersion; entry: ReleaseNotesEntry }
  >();
  for (const [version, rawEntry] of Object.entries(artifact)) {
    const parsedVersion = parseVersion(version);
    if (!parsedVersion) {
      return {
        kind: "integrity_error",
        reason: `invalid release-notes version: ${version}`,
      };
    }
    const entry = parseEntry(version, rawEntry);
    if (!entry) {
      return {
        kind: "integrity_error",
        reason: `malformed release-notes entry: ${version}`,
      };
    }
    parsedEntries.set(version, { version: parsedVersion, entry });
  }

  if (!parsedEntries.has(currentVersion)) {
    return {
      kind: "integrity_error",
      reason: `missing release-notes entry for running version: ${currentVersion}`,
    };
  }

  const parsedLastSeen =
    lastSeenVersion === null ? null : parseVersion(lastSeenVersion);
  const candidates = [...parsedEntries.values()]
    .filter(({ version }) => compareVersions(version, current) <= 0)
    .filter(
      ({ version }) =>
        parsedLastSeen === null || compareVersions(version, parsedLastSeen) > 0,
    )
    .sort((a, b) => compareVersions(b.version, a.version))
    .map(({ entry }) => entry);

  const entries =
    lastSeenVersion === null
      ? candidates.slice(0, Math.max(0, firstInstallCap))
      : candidates;
  return entries.length === 0 ? { kind: "none" } : { kind: "show", entries };
}
