{{- define "memos.signup" -}}
{
  "instanceSettings": [
    {
      "name": "instance/settings/GENERAL",
      "generalSetting": {
        "disallowUserRegistration": true
      }
    }
  ],
  "lifecycle": [
    {
      "resource": "instance/settings/GENERAL",
      "ignoreChanges": [
        "generalSetting.customProfile",
        "generalSetting.disallowPasswordAuth",
        "generalSetting.disallowChangeUsername",
        "generalSetting.disallowChangeNickname",
        "generalSetting.weekStartDayOffset",
        "generalSetting.additionalStyle",
        "generalSetting.additionalScript"
      ]
    }
  ]
}
{{- end }}
