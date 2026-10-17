#!/bin/bash
# Adds the Studio's SSH key to JAMES-DESKTOP. Asks for the desktop password once.
ssh -o StrictHostKeyChecking=accept-new james@192.168.1.204 "powershell -NoProfile -Command \"Add-Content -Path C:\ProgramData\ssh\administrators_authorized_keys -Value '$(cat ~/.ssh/id_ed25519.pub)'\"" && echo "Key added."
