$TaskName = "AstralGuard"

# Stop first: unregistering alone leaves the running guard alive until logoff.
# The app's earlier names, in case an old task is still there.
foreach ($Name in @($TaskName, "PinGuard", "12VHPWR Guard")) {
    try { Stop-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue } catch {}
    try { Unregister-ScheduledTask -TaskName $Name -Confirm:$false -ErrorAction SilentlyContinue } catch {}
}

Write-Host "Uninstalled."
Write-Host "If a tray icon is still visible (manually started instance), right-click it and choose Exit."
