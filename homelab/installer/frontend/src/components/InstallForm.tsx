import { useState } from 'react'

interface Disk {
  name: string
  size: string
  mounted: boolean
}

interface InstallFormProps {
  disks: Disk[]
  showMessage: (text: string, type?: 'success' | 'error' | 'warning' | 'info') => void
  onSuccess: () => void
}

export default function InstallForm({ disks, showMessage, onSuccess }: InstallFormProps) {
  const [selectedDisk, setSelectedDisk] = useState<string>('')
  const [hostname, setHostname] = useState('homelab')
  const [timezone, setTimezone] = useState('UTC')
  const [rootSshKey, setRootSshKey] = useState('')
  const [gitRemote, setGitRemote] = useState('')
  const [installing, setInstalling] = useState(false)
  const [installComplete, setInstallComplete] = useState(false)

  const handleInstall = async (e: React.FormEvent) => {
    e.preventDefault()

    if (!selectedDisk) {
      showMessage('Please select a disk', 'warning')
      return
    }

    if (!rootSshKey) {
      showMessage('Root SSH key is required', 'warning')
      return
    }

    if (!gitRemote) {
      showMessage('Git remote URL is required', 'warning')
      return
    }

    setInstalling(true)
    try {
      const response = await fetch('/api/install', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          disk: selectedDisk,
          hostname,
          timezone,
          root_ssh_key: rootSshKey,
          git_remote: gitRemote,
        }),
      })

      if (!response.ok) {
        const error = await response.json()
        throw new Error(error.detail || 'Installation failed')
      }

      const data = await response.json()
      setInstallComplete(true)
      showMessage(
        `Installation complete! Hostname: ${data.hostname}, Disk: ${data.disk}. You can now reboot the system.`,
        'success'
      )
      onSuccess()
    } catch (error) {
      showMessage(`Installation failed: ${error}`, 'error')
    } finally {
      setInstalling(false)
    }
  }

  if (installComplete) {
    return (
      <div className="section">
        <h2>✅ Installation Complete</h2>
        <div className="message success">
          <p><strong>Installation successful!</strong></p>
          <p>Hostname: {hostname}</p>
          <p>Disk: {selectedDisk}</p>
          <p>Git Remote: {gitRemote}</p>
          <p style={{ marginTop: '15px' }}>You can now reboot the system and remove the installation media.</p>
        </div>
      </div>
    )
  }

  return (
    <form onSubmit={handleInstall}>
      <div className="section">
        <h2>1. Select Disk</h2>
        <div className="message warning">
          ⚠️ All data on the selected disk will be erased!
        </div>
        <div className="disk-list">
          {disks.map((disk) => (
            <div
              key={disk.name}
              className={`disk-item ${selectedDisk === disk.name ? 'selected' : ''} ${disk.mounted ? 'mounted' : ''}`}
              onClick={() => !disk.mounted && setSelectedDisk(disk.name)}
            >
              <div className="disk-name">{disk.name}</div>
              <div className="disk-size">
                {disk.size} {disk.mounted && '(MOUNTED - UNAVAILABLE)'}
              </div>
            </div>
          ))}
        </div>
      </div>

      <div className="section">
        <h2>2. System Configuration</h2>
        <div className="form-group">
          <label>Hostname:</label>
          <input
            type="text"
            value={hostname}
            onChange={(e) => setHostname(e.target.value)}
            pattern="[a-z0-9-]+"
            title="Lowercase letters, numbers, and hyphens only"
            required
          />
        </div>

        <div className="form-group">
          <label>Timezone:</label>
          <input
            type="text"
            value={timezone}
            onChange={(e) => setTimezone(e.target.value)}
            placeholder="UTC, America/New_York, Europe/Paris, etc."
            required
          />
        </div>

        <div className="form-group">
          <label>Root SSH Key (REQUIRED):</label>
          <input
            type="text"
            value={rootSshKey}
            onChange={(e) => setRootSshKey(e.target.value)}
            placeholder="ssh-ed25519 AAAA..."
            required
          />
        </div>

        <div className="form-group">
          <label>Git Remote URL (REQUIRED):</label>
          <input
            type="url"
            value={gitRemote}
            onChange={(e) => setGitRemote(e.target.value)}
            placeholder="https://github.com/username/homelab.git"
            required
          />
        </div>

        <div className="message info">
          💡 Configuration will be cloned from this git repository. Make sure it's accessible!
        </div>
      </div>

      <div className="section">
        <h2>3. Install</h2>
        <button type="submit" disabled={installing}>
          {installing ? '🔄 Installing...' : '🚀 Install NixOS'}
        </button>
      </div>

      {installing && (
        <div className="progress">
          <div className="progress-step active">
            Installing NixOS... This may take several minutes.
          </div>
        </div>
      )}
    </form>
  )
}
