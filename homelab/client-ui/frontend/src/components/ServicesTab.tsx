import { useState, useEffect } from 'react'

const API_BASE = ''

interface AvailableService {
  name: string
  description?: string
}

interface DownloadedService {
  name: string
  has_compose: boolean
  has_caddy: boolean
}

interface ServicesTabProps {
  showMessage: (text: string, type?: 'success' | 'error') => void
}

export default function ServicesTab({ showMessage }: ServicesTabProps) {
  const [availableServices, setAvailableServices] = useState<AvailableService[]>([])
  const [downloadedServices, setDownloadedServices] = useState<DownloadedService[]>([])
  const [loading, setLoading] = useState(true)

  useEffect(() => {
    loadServices()
  }, [])

  const loadServices = async () => {
    setLoading(true)
    try {
      const [available, downloaded] = await Promise.all([
        fetch(`${API_BASE}/services/available`).then(r => r.json()),
        fetch(`${API_BASE}/services/downloaded`).then(r => r.json())
      ])
      setAvailableServices(available)
      setDownloadedServices(downloaded)
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error'
      showMessage(`Failed to load services: ${errorMessage}`, 'error')
    } finally {
      setLoading(false)
    }
  }

  const handleDownload = async (name: string) => {
    try {
      const response = await fetch(`${API_BASE}/services/download/${name}`, {
        method: 'POST'
      })

      if (response.ok) {
        showMessage(`Service ${name} downloaded successfully`)
        loadServices()
      } else {
        throw new Error('Download failed')
      }
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error'
      showMessage(`Failed to download service: ${errorMessage}`, 'error')
    }
  }

  const handleDelete = async (name: string) => {
    if (!confirm(`Delete service ${name}?`)) return

    try {
      const response = await fetch(`${API_BASE}/services/delete/${name}`, {
        method: 'POST'
      })

      if (response.ok) {
        showMessage(`Service ${name} deleted successfully`)
        loadServices()
      } else {
        throw new Error('Delete failed')
      }
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error'
      showMessage(`Failed to delete service: ${errorMessage}`, 'error')
    }
  }

  if (loading) {
    return (
      <div className="empty-state">
        <div className="spinner" style={{ width: 48, height: 48 }}></div>
        <p>Loading services...</p>
      </div>
    )
  }

  return (
    <>
      <div className="section-header">
        <h2>Available Services</h2>
      </div>

      {availableServices.length === 0 ? (
        <div className="empty-state">
          <p>No services available from platform</p>
        </div>
      ) : (
        <div className="service-grid">
          {availableServices.map(service => (
            <div key={service.name} className="service-card">
              <h3>{service.name}</h3>
              <p>{service.description || 'No description'}</p>
              <button onClick={() => handleDownload(service.name)}>
                Download
              </button>
            </div>
          ))}
        </div>
      )}

      <div className="section-header" style={{ marginTop: 50 }}>
        <h2>Downloaded Services</h2>
      </div>

      {downloadedServices.length === 0 ? (
        <div className="empty-state">
          <p>No services downloaded yet</p>
        </div>
      ) : (
        <div className="service-grid">
          {downloadedServices.map(service => (
            <div key={service.name} className="service-card">
              <h3>{service.name}</h3>
              <div className="status">
                <span>Docker Compose: {service.has_compose ? '✓' : '✗'}</span>
                <span>Caddyfile: {service.has_caddy ? '✓' : '✗'}</span>
              </div>
              <button className="danger" onClick={() => handleDelete(service.name)}>
                Delete
              </button>
            </div>
          ))}
        </div>
      )}
    </>
  )
}
