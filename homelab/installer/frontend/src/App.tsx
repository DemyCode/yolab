import { useState, useEffect } from 'react'
import WifiSetup from './components/WifiSetup'
import InstallForm from './components/InstallForm'

type MessageType = 'success' | 'error' | 'warning' | 'info'

interface Message {
  text: string
  type: MessageType
}

interface Status {
  internet: boolean
  disks: Disk[]
}

interface Disk {
  name: string
  size: string
  mounted: boolean
}

export default function App() {
  const [status, setStatus] = useState<Status | null>(null)
  const [message, setMessage] = useState<Message | null>(null)
  const [loading, setLoading] = useState(true)

  const showMessage = (text: string, type: MessageType = 'success') => {
    setMessage({ text, type })
    setTimeout(() => setMessage(null), 5000)
  }

  const fetchStatus = async () => {
    try {
      const response = await fetch('/api/status')
      if (!response.ok) throw new Error('Failed to fetch status')
      const data = await response.json()
      setStatus(data)
    } catch (error) {
      showMessage(`Failed to fetch status: ${error}`, 'error')
    } finally {
      setLoading(false)
    }
  }

  useEffect(() => {
    fetchStatus()
  }, [])

  if (loading) {
    return (
      <div className="container">
        <div className="loading">Loading installer...</div>
      </div>
    )
  }

  return (
    <div className="container">
      <h1>🖥️ YoLab Homelab Installer</h1>

      {status && (
        <div className={`status-bar ${status.internet ? 'connected' : 'disconnected'}`}>
          Internet Status: {status.internet ? '🟢 Connected' : '🔴 No Internet'}
        </div>
      )}

      {message && (
        <div className={`message ${message.type}`}>
          {message.text}
        </div>
      )}

      {status && !status.internet ? (
        <WifiSetup onConnected={() => {
          fetchStatus()
          showMessage('WiFi connected successfully!', 'success')
        }} showMessage={showMessage} />
      ) : (
        status && <InstallForm disks={status.disks} showMessage={showMessage} onSuccess={fetchStatus} />
      )}
    </div>
  )
}
