#!/bin/bash

APP="/Applications/Carrel.app"

echo "Carrel macOS first-run helper"
echo "=============================="
echo

if [[ ! -d "$APP" ]]; then
  echo "Carrel.app was not found in /Applications."
  echo "Drag Carrel.app from the DMG into Applications, then run this helper again."
  echo
  read -r -p "Press Return to close this window."
  exit 1
fi

echo "Removing the macOS extended attributes that prevent Carrel from opening..."

if xattr -cr "$APP"; then
  echo "Done. Opening Carrel..."
  open "$APP"
  exit 0
fi

echo
echo "Carrel could not be unlocked automatically."
echo "Open Terminal and run:"
echo
echo "  xattr -cr /Applications/Carrel.app"
echo
read -r -p "Press Return to close this window."
exit 1
