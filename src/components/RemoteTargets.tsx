/**
 * The saved list of remote destinations, and the editor for one.
 *
 * A destination cannot be chosen from a folder picker, so unlike every other
 * "where does this go" control in the app it has to be *configured* before it
 * can be used. The editor exists to make that as close to picking as possible:
 * you choose a service, and the fields it asks for are the fields that service
 * actually needs — an account ID for R2, a region for AWS, nothing but a
 * hostname for a machine you already reach over SSH.
 *
 * ## Three rules this panel is built on
 *
 * 1. **A secret is write-only.** Nothing here ever receives a stored password
 *    or key, so the field renders empty with "a key is saved" beside it.
 *    Leaving it empty keeps what is stored; clearing it explicitly removes it.
 *    That is why editing a folder does not require re-typing a key.
 * 2. **SFTP has no password field at all.** It authenticates through
 *    ssh-agent and `~/.ssh/config` exactly as `ssh` does, so asking for a
 *    credential this app would then have to hold would be inventing a secret
 *    that need not exist.
 * 3. **Remove means forget.** It deletes a bookmark and its keychain entry. It
 *    does not touch one byte at the destination, and it says so, because a
 *    Remove button next to 400 GB of backups is a button people hesitate over
 *    for good reason.
 */

import { AlertTriangle, Check, Loader2, Plus, Radio, Trash2 } from "lucide-react";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import {
  useDeleteRemoteTarget,
  useProbeRemoteTarget,
  useRemoteProfiles,
  useRemoteTargets,
  useSaveRemoteTarget,
} from "@/lib/queries";
import type {
  ProfileField,
  RemoteProfileView,
  RemoteTargetRow,
  RemoteTargetView,
  SecretInputView,
} from "@/lib/ipc";
import { cn } from "@/lib/utils";

const KIND_LABEL: Record<string, string> = {
  s3: "S3",
  web_dav: "WebDAV",
  sftp: "SFTP",
};

/** A blank target for a profile, with the profile's own defaults applied. */
function blankTarget(profile: RemoteProfileView): RemoteTargetView {
  return {
    name: "",
    kind: profile.kind,
    endpoint: profile.endpointTemplate.includes("{}") ? "" : profile.endpointTemplate,
    bucket: "",
    region: profile.region,
    root: "/",
    user: "",
  };
}

/** Fills a profile's endpoint template with the fragment the user supplied. */
function endpointFor(profile: RemoteProfileView, fragment: string): string {
  const trimmed = fragment.trim();
  if (profile.endpointTemplate.includes("{}")) return profile.endpointTemplate.replace("{}", trimmed);
  if (profile.endpointTemplate.length === 0) return trimmed;
  return profile.endpointTemplate;
}

/** What to call the endpoint field, given what the profile does with it. */
function endpointLabel(profile: RemoteProfileView): string {
  if (profile.id === "s3-r2") return "Account ID";
  if (profile.endpointTemplate.includes("{}")) return "Region or host";
  return "Server address";
}

export function RemoteTargets({ className }: { className?: string }) {
  const targets = useRemoteTargets();
  const profiles = useRemoteProfiles();
  const [editing, setEditing] = useState<{ target: RemoteTargetView; replacing: string | null } | null>(null);

  const rows = targets.data ?? [];

  return (
    <section className={cn("flex flex-col gap-2", className)}>
      <header className="flex items-center justify-between">
        <div>
          <h3 className="text-sm font-medium">Destinations</h3>
          <p className="text-xs text-muted-foreground">
            Somewhere to put files that is not a disk on this machine.
          </p>
        </div>
        {editing === null && (
          <Button
            variant="outline"
            size="sm"
            disabled={profiles.data === undefined}
            onClick={() => {
              const first = profiles.data?.[0];
              if (first !== undefined) setEditing({ target: blankTarget(first), replacing: null });
            }}
          >
            <Plus aria-hidden />
            Add
          </Button>
        )}
      </header>

      {targets.isError && (
        <Alert variant="destructive">
          <AlertTriangle aria-hidden />
          <AlertTitle>The destination list could not be read</AlertTitle>
          <AlertDescription>{targets.error.message}</AlertDescription>
        </Alert>
      )}

      {rows.length === 0 && editing === null && !targets.isLoading && (
        <p className="rounded border border-dashed border-border/60 p-4 text-center text-xs text-muted-foreground">
          No destinations yet. Add an S3 bucket, a WebDAV folder, or a server you reach over SSH.
        </p>
      )}

      {rows.length > 0 && (
        <ul className="flex flex-col gap-1">
          {rows.map((row) => (
            <TargetRow
              key={row.name}
              row={row}
              onEdit={() => setEditing({ target: { ...row }, replacing: row.name })}
            />
          ))}
        </ul>
      )}

      {editing !== null && profiles.data !== undefined && (
        <TargetEditor
          profiles={profiles.data}
          initial={editing.target}
          replacing={editing.replacing}
          onClose={() => setEditing(null)}
        />
      )}
    </section>
  );
}

function TargetRow({ row, onEdit }: { row: RemoteTargetRow; onEdit: () => void }) {
  const probe = useProbeRemoteTarget();
  const remove = useDeleteRemoteTarget();
  const [confirmingRemoval, setConfirmingRemoval] = useState(false);

  const address =
    row.kind === "s3" ? `s3://${row.bucket}${row.root}` : `${row.endpoint}${row.root}`;

  return (
    <li className="rounded border border-border/60 px-3 py-2 text-xs">
      <div className="flex items-center gap-2">
        <span className="w-16 shrink-0 text-[10px] uppercase tracking-wide text-muted-foreground">
          {KIND_LABEL[row.kind] ?? row.kind}
        </span>
        <button type="button" className="min-w-0 flex-1 text-left" onClick={onEdit}>
          <span className="block truncate font-medium">{row.name}</span>
          <span className="block truncate font-mono text-[11px] text-muted-foreground" title={address}>
            {address}
          </span>
        </button>

        <span className="shrink-0 text-[10px] text-muted-foreground">
          {row.usesAmbientCredentials
            ? "uses your SSH keys"
            : row.hasSecret
              ? "key saved"
              : "no key saved"}
        </span>

        <Button
          variant="outline"
          size="sm"
          className="shrink-0"
          disabled={probe.isPending}
          onClick={() => probe.mutate(row.name)}
        >
          {probe.isPending ? <Loader2 aria-hidden className="animate-spin" /> : <Radio aria-hidden />}
          Test
        </Button>
        <Button
          variant="outline"
          size="sm"
          className="shrink-0"
          disabled={remove.isPending}
          onClick={() => setConfirmingRemoval(true)}
        >
          <Trash2 aria-hidden />
        </Button>
      </div>

      {probe.isSuccess && probe.variables === row.name && (
        <p className="mt-1 flex items-center gap-1 text-[11px] text-muted-foreground">
          <Check aria-hidden className="size-3" />
          Reachable, and the credentials work.
        </p>
      )}
      {probe.isError && probe.variables === row.name && (
        <p className="mt-1 text-[11px] text-destructive">{probe.error.message}</p>
      )}

      {confirmingRemoval && (
        <Alert className="mt-2">
          <AlertTriangle aria-hidden />
          <AlertTitle>Forget {row.name}?</AlertTitle>
          <AlertDescription>
            <p>
              This removes the destination from this list and deletes its saved key from your
              keychain. <strong>Nothing at {address} is touched</strong> — every file that has
              already been uploaded stays exactly where it is.
            </p>
            <div className="mt-2 flex gap-2">
              <Button size="sm" variant="outline" onClick={() => setConfirmingRemoval(false)}>
                Keep it
              </Button>
              <Button
                size="sm"
                disabled={remove.isPending}
                onClick={() => {
                  remove.mutate(row.name);
                  setConfirmingRemoval(false);
                }}
              >
                Forget it
              </Button>
            </div>
          </AlertDescription>
        </Alert>
      )}
    </li>
  );
}

function TargetEditor({
  profiles,
  initial,
  replacing,
  onClose,
}: {
  profiles: readonly RemoteProfileView[];
  initial: RemoteTargetView;
  replacing: string | null;
  onClose: () => void;
}) {
  // An existing target is edited under whichever profile matches its protocol;
  // there is no way back from a saved target to the preset it came from, and
  // storing one would be a field that could disagree with the target itself.
  const [profileId, setProfileId] = useState(
    () => profiles.find((profile) => profile.kind === initial.kind)?.id ?? profiles[0].id,
  );
  const profile = profiles.find((entry) => entry.id === profileId) ?? profiles[0];

  const [target, setTarget] = useState<RemoteTargetView>(initial);
  // Held apart from `target`, because a secret must never end up in a struct
  // that gets round-tripped through the saved list.
  const [secret, setSecret] = useState<SecretInputView>({});
  const [fragment, setFragment] = useState(replacing === null ? "" : "");

  const save = useSaveRemoteTarget();
  const asks = (field: ProfileField) => profile.required.includes(field);

  function set<K extends keyof RemoteTargetView>(key: K, value: RemoteTargetView[K]) {
    setTarget((previous) => ({ ...previous, [key]: value }));
  }

  function chooseProfile(id: string) {
    const next = profiles.find((entry) => entry.id === id);
    if (next === undefined) return;
    setProfileId(id);
    // The name survives a profile change; everything protocol-specific does
    // not, because carrying an S3 bucket into an SFTP target would save a
    // field the new protocol has no meaning for.
    setTarget({ ...blankTarget(next), name: target.name });
    setFragment("");
  }

  const submit = () => {
    const resolved: RemoteTargetView = {
      ...target,
      endpoint: asks("endpoint") ? endpointFor(profile, fragment) : target.endpoint,
    };
    save.mutate(
      { target: resolved, secret, replacing },
      { onSuccess: onClose },
    );
  };

  return (
    <div className="rounded border border-border/60 p-3">
      <h4 className="text-xs font-medium">
        {replacing === null ? "New destination" : `Edit ${replacing}`}
      </h4>

      {replacing === null && (
        <div className="mt-2 flex flex-col gap-1">
          <Field label="Service">
            <select
              value={profileId}
              onChange={(event) => chooseProfile(event.target.value)}
              className="min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 py-1 text-xs"
            >
              {profiles.map((entry) => (
                <option key={entry.id} value={entry.id}>
                  {entry.label}
                </option>
              ))}
            </select>
          </Field>
          <p className="pl-24 text-[11px] text-muted-foreground">{profile.summary}</p>
        </div>
      )}

      <div className="mt-2 flex flex-col gap-1">
        <Field label="Name">
          <Text value={target.name} onChange={(value) => set("name", value)} placeholder="Backup" />
        </Field>

        {asks("endpoint") && (
          <Field label={endpointLabel(profile)}>
            <Text
              value={replacing === null ? fragment : target.endpoint}
              onChange={replacing === null ? setFragment : (value) => set("endpoint", value)}
              placeholder={profile.endpointTemplate.includes("{}") ? "us-west-002" : "nas.local"}
              mono
            />
          </Field>
        )}
        {asks("bucket") && (
          <Field label="Bucket">
            <Text value={target.bucket} onChange={(value) => set("bucket", value)} placeholder="my-backups" mono />
          </Field>
        )}
        {asks("region") && (
          <Field label="Region">
            <Text value={target.region} onChange={(value) => set("region", value)} placeholder="us-west-2" mono />
          </Field>
        )}
        {asks("user") && (
          <Field label="User name">
            <Text value={target.user} onChange={(value) => set("user", value)} placeholder="josh" mono />
          </Field>
        )}
        {asks("root") && (
          <Field label="Folder">
            <Text value={target.root} onChange={(value) => set("root", value)} placeholder="/" mono />
          </Field>
        )}

        {asks("secret") && profile.kind === "s3" && (
          <>
            <Field label="Access key">
              <Text
                value={secret.accessKey ?? ""}
                onChange={(value) => setSecret((previous) => ({ ...previous, accessKey: value }))}
                placeholder={replacing === null ? "AKIA…" : "leave blank to keep the saved key"}
                mono
              />
            </Field>
            <Field label="Secret key">
              <Text
                value={secret.secretKey ?? ""}
                onChange={(value) => setSecret((previous) => ({ ...previous, secretKey: value }))}
                placeholder={replacing === null ? "" : "leave blank to keep the saved key"}
                secret
              />
            </Field>
          </>
        )}
        {asks("secret") && profile.kind === "web_dav" && (
          <Field label="Password">
            <Text
              value={secret.password ?? ""}
              onChange={(value) => setSecret((previous) => ({ ...previous, password: value }))}
              placeholder={replacing === null ? "" : "leave blank to keep the saved password"}
              secret
            />
          </Field>
        )}
      </div>

      {profile.kind === "s3" && !asks("secret") && (
        <p className="mt-2 pl-24 text-[11px] text-muted-foreground">
          No key needed here — this uses your existing AWS credentials: the <code>AWS_*</code>{" "}
          environment, <code>~/.aws/credentials</code>, an SSO session, or an instance role. Add one
          above only if you want this destination to use a specific key instead.
        </p>
      )}
      {profile.kind === "sftp" && (
        <p className="mt-2 pl-24 text-[11px] text-muted-foreground">
          Nothing is stored for SFTP. It uses <code>~/.ssh/config</code> and your ssh-agent, so if{" "}
          <code>ssh</code> reaches this server, so does this.
        </p>
      )}

      {save.isError && (
        <Alert variant="destructive" className="mt-2">
          <AlertTriangle aria-hidden />
          <AlertTitle>This destination cannot be saved</AlertTitle>
          <AlertDescription>{save.error.message}</AlertDescription>
        </Alert>
      )}

      <div className="mt-3 flex justify-end gap-2">
        <Button variant="outline" size="sm" onClick={onClose}>
          Cancel
        </Button>
        <Button size="sm" disabled={save.isPending || target.name.trim().length === 0} onClick={submit}>
          {save.isPending && <Loader2 aria-hidden className="animate-spin" />}
          Save
        </Button>
      </div>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center gap-2">
      <span className="w-22 shrink-0 text-xs text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

function Text({
  value,
  onChange,
  placeholder,
  mono = false,
  secret = false,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  mono?: boolean;
  secret?: boolean;
}) {
  return (
    <input
      type={secret ? "password" : "text"}
      value={value}
      placeholder={placeholder}
      spellCheck={false}
      autoComplete={secret ? "new-password" : "off"}
      onChange={(event) => onChange(event.target.value)}
      className={cn(
        "min-w-0 flex-1 rounded border border-border/60 bg-transparent px-2 py-1 text-xs focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring",
        mono && "font-mono",
      )}
    />
  );
}
