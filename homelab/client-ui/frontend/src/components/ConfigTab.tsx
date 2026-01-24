import { useState, useEffect } from 'react'

const API_BASE = ''

type TomlValue = string | number | boolean | TomlObject | TomlValue[]

interface TomlObject {
  [key: string]: TomlValue
}

interface TomlArray {
  key: string
  value: TomlObject[]
}

function toToml(obj: TomlObject, indent = ''): string {
  let result = ''
  const simple: Record<string, TomlValue> = {}
  const tables: Record<string, TomlObject> = {}
  const arrays: TomlArray[] = []

  for (const [key, value] of Object.entries(obj)) {
    if (Array.isArray(value) && value.length > 0 && typeof value[0] === 'object') {
      arrays.push({ key, value: value as TomlObject[] })
    } else if (typeof value === 'object' && !Array.isArray(value) && value !== null) {
      tables[key] = value as TomlObject
    } else {
      simple[key] = value
    }
  }

  for (const [key, value] of Object.entries(simple)) {
    if (Array.isArray(value)) {
      result += `${key} = ${JSON.stringify(value)}\n`
    } else if (typeof value === 'string') {
      result += `${key} = "${value}"\n`
    } else {
      result += `${key} = ${value}\n`
    }
  }

  for (const [key, value] of Object.entries(tables)) {
    result += `\n[${key}]\n`
    result += toToml(value, indent + '  ')
  }

  for (const { key, value } of arrays) {
    for (const item of value) {
      result += `\n[[${key}]]\n`
      result += toToml(item, indent + '  ')
    }
  }

  return result
}

function parseToml(text: string): TomlObject {
  const lines = text.split('\n')
  const result: TomlObject = {}
  let currentSection: TomlObject = result
  let currentPath: string[] = []
  let currentArray: TomlObject | null = null

  for (let line of lines) {
    line = line.trim()
    if (!line || line.startsWith('#')) continue

    if (line.startsWith('[[') && line.endsWith(']]')) {
      const arrayName = line.slice(2, -2)
      if (!Array.isArray(result[arrayName])) {
        result[arrayName] = []
      }
      currentArray = {}
      ;(result[arrayName] as TomlObject[]).push(currentArray)
      currentSection = currentArray
    } else if (line.startsWith('[') && line.endsWith(']')) {
      const section = line.slice(1, -1)
      currentPath = section.split('.')
      currentSection = result
      currentArray = null

      for (const part of currentPath) {
        if (!(part in currentSection)) {
          currentSection[part] = {}
        }
        currentSection = currentSection[part] as TomlObject
      }
    } else if (line.includes('=')) {
      const [key, ...valueParts] = line.split('=')
      const value = valueParts.join('=').trim()

      try {
        currentSection[key.trim()] = JSON.parse(value)
      } catch {
        currentSection[key.trim()] = value.replace(/^"|"$/g, '')
      }
    }
  }

  return result
}

interface ConfigTabProps {
  showMessage: (text: string, type?: 'success' | 'error') => void
}

export default function ConfigTab({ showMessage }: ConfigTabProps) {
  const [config, setConfig] = useState('')
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)

  useEffect(() => {
    loadConfig()
  }, [])

  const loadConfig = async () => {
    setLoading(true)
    try {
      const response = await fetch(`${API_BASE}/config`)
      const data = await response.json()
      setConfig(toToml(data))
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error'
      showMessage(`Failed to load configuration: ${errorMessage}`, 'error')
    } finally {
      setLoading(false)
    }
  }

  const saveConfig = async () => {
    setSaving(true)
    try {
      const parsed = parseToml(config)
      const response = await fetch(`${API_BASE}/config`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(parsed)
      })

      if (response.ok) {
        showMessage('Configuration saved successfully')
      } else {
        throw new Error('Failed to save')
      }
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error'
      showMessage(`Failed to save configuration: ${errorMessage}`, 'error')
    } finally {
      setSaving(false)
    }
  }

  if (loading) {
    return (
      <div className="empty-state">
        <div className="spinner" style={{ width: 48, height: 48 }}></div>
        <p>Loading configuration...</p>
      </div>
    )
  }

  return (
    <>
      <div className="section-header">
        <h2>Configuration (config.toml)</h2>
        <button onClick={saveConfig} disabled={saving}>
          {saving ? 'Saving...' : 'Save Configuration'}
        </button>
      </div>

      <textarea
        value={config}
        onChange={(e) => setConfig(e.target.value)}
        placeholder="Edit your configuration..."
      />
    </>
  )
}
