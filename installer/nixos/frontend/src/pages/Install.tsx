import { useEffect, useRef, useState } from 'react'

type Disk = {
  name: string
  size: string
  tran: string
  is_usb: boolean
  mounted: boolean
  recommended?: boolean
}

type Step = 1 | 2 | 3 | 4 | 5

type JoinInfo = {
  server_addr: string
  k3s_token: string
  nodeUrl: string
}

// ── Shared UI ──────────────────────────────────────────────────────────────────

function Card({ children }: { children: React.ReactNode }) {
  return (
    <div className="bg-surface border border-border rounded-2xl p-6 w-full max-w-lg">
      {children}
    </div>
  )
}

function PrimaryBtn({
  children,
  onClick,
  disabled,
}: {
  children: React.ReactNode
  onClick?: () => void
  disabled?: boolean
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className="w-full py-3 bg-accent text-black font-bold rounded-lg hover:opacity-90 disabled:opacity-40 transition-opacity mt-5"
    >
      {children}
    </button>
  )
}

function SecondaryBtn({ children, onClick }: { children: React.ReactNode; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="w-full py-2 border border-border text-muted rounded-lg hover:border-[#444] hover:text-[#e5e7eb] transition-colors mt-2 text-sm"
    >
      {children}
    </button>
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="mt-4">
      <label className="block text-xs font-semibold text-muted mb-1">{label}</label>
      {children}
    </div>
  )
}

function Input(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      {...props}
      className="w-full bg-bg border border-border rounded-lg px-3 py-2 text-sm text-[#e5e7eb] outline-none focus:border-accent transition-colors"
    />
  )
}

function ErrorMsg({ msg }: { msg: string }) {
  if (!msg) return null
  return (
    <div className="mt-3 bg-[#1f0a0a] border border-[#7f1d1d] text-[#f87171] text-sm rounded-lg px-3 py-2">
      {msg}
    </div>
  )
}

function StepHeader({ step, title }: { step: Step; title: string }) {
  return (
    <div className="flex items-center gap-3 mb-5">
      <div className="w-7 h-7 rounded-full bg-[#1a2e26] border border-accent text-accent text-xs font-bold flex items-center justify-center shrink-0">
        {step}
      </div>
      <h2 className="font-bold">{title}</h2>
      <span className="ml-auto text-xs text-muted">Step {step} / 5</span>
    </div>
  )
}

// ── Step 1: Cluster mode ───────────────────────────────────────────────────────

function StepCluster({ onNew, onJoin }: { onNew: () => void; onJoin: (info: JoinInfo) => void }) {
  const [mode, setMode] = useState<'new' | 'join' | null>(null)
  // Account sub-step (new cluster only)
  const [accountMethod, setAccountMethod] = useState<'create' | 'existing' | null>(null)
  const [existingToken, setExistingToken] = useState('')
  const [createdToken, setCreatedToken] = useState('')
  const [accountReady, setAccountReady] = useState(false)
  // Join form
  const [url, setUrl] = useState('')
  const [password, setPassword] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState('')

  async function createAccount() {
    setError('')
    setLoading(true)
    try {
      const res = await fetch('/api/account/create', { method: 'POST' })
      const data = await res.json()
      if (!res.ok) { setError(data.detail ?? `Error ${res.status}`); return }
      setCreatedToken(data.account_token)
      setAccountReady(true)
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }

  async function verifyToken() {
    setError('')
    if (!existingToken.trim()) { setError('Token is required'); return }
    setLoading(true)
    try {
      const res = await fetch('/api/account/verify', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ token: existingToken.trim() }),
      })
      const data = await res.json()
      if (!res.ok) { setError(data.detail ?? `Error ${res.status}`); return }
      setAccountReady(true)
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }

  async function fetchJoinInfo() {
    setError('')
    if (!url.trim()) { setError('Node URL is required'); return }
    if (!password) { setError('Password is required'); return }
    setLoading(true)
    try {
      const res = await fetch('/api/join-info', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ url: url.trim(), password }),
      })
      const data = await res.json()
      if (!res.ok) {
        setError(data.detail ?? `Server error ${res.status}`)
        return
      }
      onJoin({ server_addr: data.server_addr, k3s_token: data.k3s_token, nodeUrl: url.trim() })
    } catch (e) {
      setError(String(e))
    } finally {
      setLoading(false)
    }
  }

  function selectMode(m: 'new' | 'join') {
    setMode(m)
    setError('')
    setAccountMethod(null)
    setAccountReady(false)
    setCreatedToken('')
  }

  return (
    <Card>
      <StepHeader step={1} title="Cluster setup" />

      <div className="flex flex-col gap-3">
        <button
          onClick={() => selectMode('new')}
          className={`flex flex-col gap-1 p-4 rounded-xl border text-left transition-colors ${mode === 'new' ? 'border-accent bg-[#0a1f15]' : 'border-border hover:border-[#444]'}`}
        >
          <span className="text-sm font-semibold text-[#e5e7eb]">New cluster</span>
          <span className="text-xs text-muted">First node — creates a fresh single-node cluster</span>
        </button>

        <button
          onClick={() => selectMode('join')}
          className={`flex flex-col gap-1 p-4 rounded-xl border text-left transition-colors ${mode === 'join' ? 'border-accent bg-[#0a1f15]' : 'border-border hover:border-[#444]'}`}
        >
          <span className="text-sm font-semibold text-[#e5e7eb]">Join existing cluster</span>
          <span className="text-xs text-muted">Additional node — joins a running YoLab cluster</span>
        </button>
      </div>

      {/* New cluster: account setup */}
      {mode === 'new' && !accountReady && (
        <div className="mt-5">
          <p className="text-xs text-muted mb-3">
            Connect your YoLab account so each node gets its own public URL.
          </p>
          <div className="flex gap-2 mb-4">
            <button
              onClick={() => setAccountMethod('create')}
              className={`flex-1 py-2 text-xs rounded-lg border transition-colors ${accountMethod === 'create' ? 'border-accent text-accent bg-[#0a1f15]' : 'border-border text-muted hover:border-[#444]'}`}
            >
              Create new account
            </button>
            <button
              onClick={() => setAccountMethod('existing')}
              className={`flex-1 py-2 text-xs rounded-lg border transition-colors ${accountMethod === 'existing' ? 'border-accent text-accent bg-[#0a1f15]' : 'border-border text-muted hover:border-[#444]'}`}
            >
              I have an account
            </button>
          </div>
          {accountMethod === 'create' && (
            <>
              <p className="text-xs text-muted mb-3">
                A new account is created instantly — no email required.
              </p>
              <PrimaryBtn onClick={createAccount} disabled={loading}>
                {loading ? 'Creating…' : 'Create account →'}
              </PrimaryBtn>
            </>
          )}
          {accountMethod === 'existing' && (
            <>
              <Field label="Account token">
                <Input
                  value={existingToken}
                  onChange={(e) => setExistingToken(e.target.value)}
                  placeholder="Paste your YoLab account token"
                />
              </Field>
              <PrimaryBtn onClick={verifyToken} disabled={loading}>
                {loading ? 'Verifying…' : 'Verify token →'}
              </PrimaryBtn>
            </>
          )}
          <ErrorMsg msg={error} />
        </div>
      )}

      {/* New cluster: account ready */}
      {mode === 'new' && accountReady && (
        <div className="mt-5">
          {createdToken && (
            <div className="mb-4 bg-[#0d2118] border border-[#065f46] rounded-lg p-3">
              <p className="text-xs text-[#86efac] mb-2 font-semibold">
                Save this token — you'll need it to add more nodes later:
              </p>
              <pre className="text-xs font-mono text-accent break-all whitespace-pre-wrap">{createdToken}</pre>
            </div>
          )}
          <p className="text-sm text-[#86efac] mb-4">✓ Account ready</p>
          <ErrorMsg msg={error} />
          <PrimaryBtn onClick={onNew}>Continue →</PrimaryBtn>
        </div>
      )}

      {/* Join cluster form */}
      {mode === 'join' && (
        <div>
          <Field label="Existing node URL">
            <Input
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder="https://my-homelab.example.com"
              type="url"
            />
          </Field>
          <Field label="Homelab password">
            <Input
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              type="password"
              placeholder="Password of the existing node"
            />
          </Field>
          <ErrorMsg msg={error} />
          <PrimaryBtn onClick={fetchJoinInfo} disabled={loading}>
            {loading ? 'Connecting…' : 'Connect & Continue →'}
          </PrimaryBtn>
        </div>
      )}
    </Card>
  )
}

// ── Step 2: Disk ───────────────────────────────────────────────────────────────

function StepDisk({ onNext, onBack }: { onNext: (disk: string) => void; onBack: () => void }) {
  const [disks, setDisks] = useState<Disk[] | null>(null)
  const [selected, setSelected] = useState('')
  const [error, setError] = useState('')

  useEffect(() => {
    fetch('/api/disks')
      .then((r) => {
        if (!r.ok) throw new Error(`Server error: ${r.status}`)
        return r.json()
      })
      .then((data: Disk[]) => {
        setDisks(data)
        const rec = data.find((d) => d.recommended)
        if (rec) setSelected(rec.name)
      })
      .catch((e) => setError(e.message || 'Failed to load disks'))
  }, [])

  function proceed() {
    if (!selected) { setError('Please select a disk'); return }
    onNext(selected)
  }

  return (
    <Card>
      <StepHeader step={2} title="Select installation disk" />
      <p className="text-[#f87171] text-xs mb-4">⚠ The selected disk will be completely erased.</p>

      {disks === null && !error && <p className="text-muted text-sm">Detecting disks…</p>}
      {disks !== null && disks.length === 0 && !error && (
        <p className="text-muted text-sm">No disks detected. Make sure the drive is connected.</p>
      )}

      <div className="flex flex-col gap-2">
        {(disks ?? []).map((d) => (
          <label
            key={d.name}
            className={`flex items-center gap-3 p-3 rounded-xl border cursor-pointer transition-colors ${selected === d.name ? 'border-accent bg-[#0a1f15]' : 'border-border hover:border-[#444]'
              } ${d.mounted ? 'opacity-40 cursor-not-allowed' : ''}`}
          >
            <input
              type="radio"
              name="disk"
              value={d.name}
              checked={selected === d.name}
              disabled={d.mounted}
              onChange={() => setSelected(d.name)}
              className="accent-accent"
            />
            <div>
              <div className="text-sm font-mono font-semibold">
                {d.name} &nbsp; {d.size}
                {d.recommended && (
                  <span className="ml-2 text-xs bg-[#0d2118] border border-[#065f46] text-accent px-2 py-0.5 rounded-full">
                    ★ Recommended
                  </span>
                )}
                {d.is_usb && (
                  <span className="ml-2 text-xs bg-[#1a1a2e] border border-[#3730a3] text-[#818cf8] px-2 py-0.5 rounded-full">
                    USB
                  </span>
                )}
              </div>
              <div className="text-xs text-muted">{d.tran} {d.mounted ? '· in use' : ''}</div>
            </div>
          </label>
        ))}
      </div>

      <ErrorMsg msg={error} />
      <PrimaryBtn onClick={proceed} disabled={!selected}>Continue →</PrimaryBtn>
      <SecondaryBtn onClick={onBack}>← Back</SecondaryBtn>
    </Card>
  )
}

// ── Step 3: Config ─────────────────────────────────────────────────────────────

type Config = {
  hostname: string
  timezone: string
  password: string
  password2: string
  sshKey: string
}

function StepConfig({ onNext, onBack }: { onNext: (c: Config) => void; onBack: () => void }) {
  const [form, setForm] = useState<Config>({
    hostname: 'homelab',
    timezone: 'UTC',
    password: '',
    password2: '',
    sshKey: '',
  })
  const [privateKey, setPrivateKey] = useState('')
  const [error, setError] = useState('')
  const [generating, setGenerating] = useState(false)

  function set(k: keyof Config) {
    return (e: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) =>
      setForm((f) => ({ ...f, [k]: e.target.value }))
  }

  async function generateKey() {
    setGenerating(true)
    try {
      const res = await fetch('/api/generate-ssh-key', { method: 'POST' })
      const data = await res.json()
      setForm((f) => ({ ...f, sshKey: data.public_key }))
      setPrivateKey(data.private_key)
    } catch {
      setError('Failed to generate SSH key')
    } finally {
      setGenerating(false)
    }
  }

  function proceed() {
    if (!form.hostname.trim()) { setError('Hostname is required'); return }
    if (form.password.length < 8) { setError('Password must be at least 8 characters'); return }
    if (form.password !== form.password2) { setError('Passwords do not match'); return }
    setError('')
    onNext(form)
  }

  return (
    <Card>
      <StepHeader step={3} title="System configuration" />

      <Field label="Hostname">
        <Input value={form.hostname} onChange={set('hostname')} maxLength={20} />
      </Field>
      <Field label="Timezone">
        <Input value={form.timezone} onChange={set('timezone')} placeholder="e.g. Europe/Paris" />
      </Field>
      <Field label="Password">
        <Input type="password" value={form.password} onChange={set('password')} placeholder="At least 8 characters" />
        <Input type="password" value={form.password2} onChange={set('password2')} placeholder="Confirm password" className="mt-1 w-full bg-bg border border-border rounded-lg px-3 py-2 text-sm text-[#e5e7eb] outline-none focus:border-accent transition-colors" />
      </Field>
      <Field label="SSH public key (optional)">
        <textarea
          value={form.sshKey}
          onChange={set('sshKey')}
          placeholder="ssh-ed25519 AAAA…"
          rows={3}
          className="w-full bg-bg border border-border rounded-lg px-3 py-2 text-sm font-mono text-[#e5e7eb] outline-none focus:border-accent transition-colors resize-none"
        />
        <button
          onClick={generateKey}
          disabled={generating}
          className="mt-1 text-xs text-muted hover:text-accent transition-colors"
        >
          {generating ? 'Generating…' : 'Generate key pair'}
        </button>
      </Field>

      {privateKey && (
        <div className="mt-3 bg-bg border border-[#7f1d1d] rounded-lg p-3">
          <p className="text-xs text-[#f87171] mb-2 font-semibold">Save this private key — it will not be shown again:</p>
          <pre className="text-xs font-mono text-accent break-all whitespace-pre-wrap">{privateKey}</pre>
        </div>
      )}

      <ErrorMsg msg={error} />
      <PrimaryBtn onClick={proceed}>Continue →</PrimaryBtn>
      <SecondaryBtn onClick={onBack}>← Back</SecondaryBtn>
    </Card>
  )
}

// ── Step 4: Confirm ────────────────────────────────────────────────────────────

function StepConfirm({
  disk,
  config,
  joinInfo,
  onInstall,
  onBack,
}: {
  disk: string
  config: Config
  joinInfo: JoinInfo | null
  onInstall: () => void
  onBack: () => void
}) {
  return (
    <Card>
      <StepHeader step={4} title="Confirm & Install" />

      <div className="text-sm text-muted space-y-2">
        <div className="flex justify-between">
          <span>Mode</span>
          <span className="text-[#e5e7eb]">{joinInfo ? 'Join existing cluster' : 'New cluster'}</span>
        </div>
        {joinInfo && (
          <div className="flex justify-between">
            <span>Joining</span>
            <span className="text-[#e5e7eb] font-mono text-xs truncate max-w-[60%]">{joinInfo.nodeUrl}</span>
          </div>
        )}
        <div className="flex justify-between"><span>Disk</span><span className="text-[#f87171] font-mono">{disk}</span></div>
        <div className="flex justify-between"><span>Hostname</span><span className="text-[#e5e7eb]">{config.hostname}</span></div>
        <div className="flex justify-between"><span>Timezone</span><span className="text-[#e5e7eb]">{config.timezone}</span></div>
        <div className="flex justify-between"><span>SSH key</span><span className="text-[#e5e7eb]">{config.sshKey ? 'provided' : 'none'}</span></div>
      </div>

      <p className="mt-4 text-xs text-[#f87171]">
        This will permanently erase <strong>{disk}</strong> and install NixOS. This cannot be undone.
      </p>

      <PrimaryBtn onClick={onInstall}>Install YoLab</PrimaryBtn>
      <SecondaryBtn onClick={onBack}>← Back</SecondaryBtn>
    </Card>
  )
}

// ── Step 5: Progress ───────────────────────────────────────────────────────────

function StepProgress() {
  const [lines, setLines] = useState<string[]>([])
  const [done, setDone] = useState(false)
  const [failed, setFailed] = useState(false)
  const logRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    const es = new EventSource('/api/progress')
    es.onmessage = (e) => {
      if (e.data === '__DONE__') { es.close(); setDone(true); return }
      if (e.data === '__ERROR__') { es.close(); setFailed(true); return }
      if (e.data.trim()) setLines((l) => [...l, e.data])
    }
    return () => es.close()
  }, [])

  useEffect(() => {
    if (logRef.current) logRef.current.scrollTop = logRef.current.scrollHeight
  }, [lines])

  return (
    <Card>
      <StepHeader step={5} title="Installing…" />
      <p className="text-xs text-muted mb-3">This takes 5–15 minutes. Keep this page open.</p>

      <div
        ref={logRef}
        className="bg-black border border-border rounded-lg p-3 font-mono text-xs text-[#d1fae5] h-72 overflow-y-auto whitespace-pre-wrap break-all"
      >
        {lines.map((l, i) => <div key={i}>{l}</div>)}
      </div>

      {done && (
        <div className="mt-6 rounded-lg border border-[#86efac] bg-[#0d1f14] p-4 text-sm space-y-2">
          <p className="text-[#86efac] font-bold">✓ Installation complete!</p>
          <ol className="text-[#ccc] space-y-1 list-decimal list-inside">
            <li>Power off the computer</li>
            <li>Remove the installation USB stick</li>
            <li>Turn on the machine again</li>
          </ol>
        </div>
      )}
      {failed && (
        <p className="mt-4 text-[#f87171] font-bold text-sm">
          ✗ Installation failed. See the log above for details.
        </p>
      )}
    </Card>
  )
}

// ── Root ───────────────────────────────────────────────────────────────────────

export default function Install() {
  const [step, setStep] = useState<Step>(1)
  const [joinInfo, setJoinInfo] = useState<JoinInfo | null>(null)
  const [disk, setDisk] = useState('')
  const [config, setConfig] = useState<Config | null>(null)
  const [installError, setInstallError] = useState('')

  async function startInstall() {
    if (!config) return
    setInstallError('')
    try {
      const res = await fetch('/api/install', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          disk,
          hostname: config.hostname,
          timezone: config.timezone,
          password: config.password,
          ssh_key: config.sshKey,
          ...(joinInfo && {
            server_addr: joinInfo.server_addr,
            k3s_token: joinInfo.k3s_token,
          }),
        }),
      })
      if (res.ok) {
        setStep(5)
      } else {
        const body = await res.json().catch(() => ({}))
        setInstallError(body.detail ?? `Server error ${res.status}`)
      }
    } catch (e) {
      setInstallError(String(e))
    }
  }

  return (
    <div className="min-h-screen bg-bg flex flex-col items-center justify-center p-6 gap-4">
      <div className="text-xl font-extrabold">
        Yo<span className="text-accent">Lab</span>{' '}
        <span className="text-muted text-sm font-normal">Installer</span>
      </div>

      {step === 1 && (
        <StepCluster
          onNew={() => { setJoinInfo(null); setStep(2) }}
          onJoin={(info) => { setJoinInfo(info); setStep(2) }}
        />
      )}
      {step === 2 && (
        <StepDisk
          onNext={(d) => { setDisk(d); setStep(3) }}
          onBack={() => setStep(1)}
        />
      )}
      {step === 3 && (
        <StepConfig
          onNext={(c) => { setConfig(c); setStep(4) }}
          onBack={() => setStep(2)}
        />
      )}
      {step === 4 && config && (
        <>
          {installError && (
            <p className="text-[#f87171] text-sm">{installError}</p>
          )}
          <StepConfirm
            disk={disk}
            config={config}
            joinInfo={joinInfo}
            onInstall={startInstall}
            onBack={() => setStep(3)}
          />
        </>
      )}
      {step === 5 && <StepProgress />}
    </div>
  )
}
