import { useState, useEffect } from 'react'

interface Network {
  ssid: string
  signal: string
  security: string
}

interface WifiSetupProps {
  onConnected: () => void
  showMessage: (text: string, type?: 'success' | 'error' | 'warning' | 'info') => void
}

export default function WifiSetup({ onConnected, showMessage }: WifiSetupProps) {
  const [networks, setNetworks] = useState<Network[]>([])
  const [selectedNetwork, setSelectedNetwork] = useState<string>('')
  const [password, setPassword] = useState('')
  const [connecting, setConnecting] = useState(false)
  const [scanning, setScanning] = useState(false)

  const scanNetworks = async () => {
    setScanning(true)
    try {
      const response = await fetch('/api/wifi/scan')
      if (!response.ok) throw new Error('Failed to scan networks')
      const data = await response.json()
      setNetworks(data.networks)
    } catch (error) {
      showMessage(`Failed to scan networks: ${error}`, 'error')
    } finally {
      setScanning(false)
    }
  }

  useEffect(() => {
    scanNetworks()
  }, [])

  const handleConnect = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!selectedNetwork) {
      showMessage('Please select a network', 'warning')
      return
    }

    setConnecting(true)
    try {
      const response = await fetch('/api/wifi/connect', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          ssid: selectedNetwork,
          password: password,
        }),
      })

      if (!response.ok) {
        const error = await response.json()
        throw new Error(error.detail || 'Failed to connect')
      }

      onConnected()
    } catch (error) {
      showMessage(`Connection failed: ${error}`, 'error')
    } finally {
      setConnecting(false)
    }
  }

  return (
    <div className="section">
      <h2>📡 WiFi Setup Required</h2>

      <div className="message warning">
        ⚠️ Internet connection is required for installation. Please connect to WiFi or plug in an ethernet cable.
      </div>

      <form onSubmit={handleConnect}>
        <div className="form-group">
          <label>Available Networks:</label>
          <div className="wifi-networks">
            {networks.map((network) => (
              <div
                key={network.ssid}
                className={`wifi-network ${selectedNetwork === network.ssid ? 'selected' : ''}`}
                onClick={() => setSelectedNetwork(network.ssid)}
              >
                <div className="wifi-info">
                  <span>{network.security ? '🔒' : '🔓'}</span>
                  <span>{network.ssid}</span>
                </div>
                <span className="signal-strength">{network.signal}%</span>
              </div>
            ))}
          </div>
        </div>

        {selectedNetwork && (
          <div className="form-group">
            <label>Password (leave empty for open networks):</label>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="WiFi password"
            />
          </div>
        )}

        <button type="submit" disabled={connecting || !selectedNetwork}>
          {connecting ? '🔄 Connecting...' : '🔌 Connect'}
        </button>

        <button
          type="button"
          onClick={scanNetworks}
          disabled={scanning}
          style={{ marginLeft: '10px' }}
        >
          {scanning ? '🔄 Scanning...' : '🔄 Rescan Networks'}
        </button>
      </form>

      <div className="message info" style={{ marginTop: '20px' }}>
        💡 If you've plugged in an ethernet cable, it will be automatically detected. Reload the page to check.
      </div>
    </div>
  )
}
