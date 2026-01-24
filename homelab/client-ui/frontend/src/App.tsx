import { useState } from 'react'
import ServicesTab from './components/ServicesTab'
import ConfigTab from './components/ConfigTab'

const API_BASE = ''

type MessageType = 'success' | 'error'

interface Message {
  text: string
  type: MessageType
}

export default function App() {
  const [activeTab, setActiveTab] = useState<'services' | 'config'>('services')
  const [message, setMessage] = useState<Message | null>(null)
  const [rebuilding, setRebuilding] = useState(false)

  const showMessage = (text: string, type: MessageType = 'success') => {
    setMessage({ text, type })
    setTimeout(() => setMessage(null), 5000)
  }

  const handleRebuild = async () => {
    if (!confirm('Rebuild the system? This will run nixos-rebuild switch.')) return

    setRebuilding(true)
    showMessage('Rebuilding system...', 'success')

    try {
      const response = await fetch(`${API_BASE}/rebuild`, { method: 'POST' })
      const result = await response.json()

      if (result.status === 'success') {
        showMessage('System rebuilt successfully')
      } else {
        showMessage(`Rebuild failed: ${result.stderr}`, 'error')
      }
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error'
      showMessage(`Failed to rebuild: ${errorMessage}`, 'error')
    } finally {
      setRebuilding(false)
    }
  }

  return (
    <>
      <div className="container">
        <header className="header">
          <h1>YoLab Client</h1>
          <div className="header-actions">
            <button onClick={handleRebuild} disabled={rebuilding}>
              {rebuilding ? (
                <>
                  <span className="spinner"></span>
                  Rebuilding...
                </>
              ) : (
                'Rebuild System'
              )}
            </button>
          </div>
        </header>

        <div className="tabs">
          <button
            className={`tab ${activeTab === 'services' ? 'active' : ''}`}
            onClick={() => setActiveTab('services')}
          >
            Services
          </button>
          <button
            className={`tab ${activeTab === 'config' ? 'active' : ''}`}
            onClick={() => setActiveTab('config')}
          >
            Configuration
          </button>
        </div>

        <div className="content">
          {message && (
            <div className={`message ${message.type}`}>
              {message.text}
            </div>
          )}

          <div className={`section ${activeTab === 'services' ? 'active' : ''}`}>
            <ServicesTab showMessage={showMessage} />
          </div>

          <div className={`section ${activeTab === 'config' ? 'active' : ''}`}>
            <ConfigTab showMessage={showMessage} />
          </div>
        </div>
      </div>
    </>
  )
}
