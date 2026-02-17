# ClipRelay Features Summary

## Room Choice on Every Launch ✅ (Just Added)

**Status:** Implemented and committed (7d17fb5)

When you launch the Windows client **without** a `--room-code` CLI argument, you now see a **Room Choice** dialog every time:

### With Saved Configuration
- Dialog shows your saved room details:
  - Room code
  - Server URL  
  - Device name
- Three buttons:
  - **"Use Saved Room"** - Instantly connects using your saved settings
  - **"Setup New Room"** - Opens the full setup dialog to create/join a different room
  - **"Cancel"** - Exits the application

### Without Saved Configuration  
- Goes directly to **"Setup New Room"** dialog
- Enter room code, server URL, and device name
- Saves to `%LOCALAPPDATA%\ClipRelay\config.json` for future use

### Benefits
- Quick access to your regular sync room
- Easy to switch rooms without CLI arguments or editing config files
- Clear choice on every launch - no hidden behavior

## File Sending ✅ (Already Implemented)

**Status:** Fully working since initial implementation

### How to Send Files

1. **Ensure Connected**: Tray icon must be **Green** (room key ready)
2. **Open Send Window**: Double-click the tray icon  
3. **Click "Send File..."** button
4. **Select File**: File dialog opens - choose any file up to **5MB**
5. **File Queued**: Tray notification confirms file queued for sending
6. **Automatic Transfer**: File is chunked (64KB chunks), encrypted, and sent to all devices in the room

### How Files Are Received

1. **Notification**: Receiving device shows tray notification
2. **Popup Window**: Displays file name and size
3. **Save Option**: Click **"Save"** to store file in `Downloads\ClipRelay\`
4. **Dismiss Option**: Click **"Dismiss"** to ignore the file

### Technical Details

- **File Size Limit**: 5MB (5,242,880 bytes)
- **Chunk Size**: 64KB per chunk
- **Encryption**: End-to-end encrypted with XChaCha20-Poly1305
- **MIME Type**: `application/x-cliprelay-file-chunk+json;base64`
- **Reassembly**: Chunks are reassembled in order on receiving device
- **Integrity**: File hash verification ensures complete transfer

### Supported Scenarios

✅ Send any file type (binary or text)  
✅ Multiple devices receive the same file simultaneously  
✅ Files larger than clipboard limit (up to 5MB)  
✅ Progress indication via tray notifications  
✅ Secure transfer through relay (relay cannot decrypt)  

### Error Handling

- Empty files rejected
- Files over 5MB rejected with clear error message  
- Network disconnection during transfer shows error notification
- Room key not ready blocks send button until encryption possible
- Invalid file paths or permission errors reported via tray notification

## UI Improvements (Previous Work)

### Window Sizing ✅
- Single tabbed window: 560×420px (default), 400×300px (minimum)
- Window starts centered on screen
- Tabs: Send | Options | Notifications
- All with consistent 16px margins to prevent button clipping

### DPI Scaling ✅
- Handled automatically by egui/eframe immediate-mode rendering
- No manual DPI conversion needed
- Supports high-DPI and 4K displays properly

### Setup Dialog Polish ✅  
- Welcome message: "Welcome! Enter your room details to get started:"
- Usage tip: "Tip: Use the same room code on multiple devices to sync clipboards."
- Centered on screen
- Clean vertical spacing  
- Button text: "Connect" (was "Start")

## Status Indicators

### Tray Icon Colors
- 🔴 **Red**: Disconnected / Cannot reach relay
- 🟡 **Amber**: Connected, but no room key yet (usually only device in room)
- 🟢 **Green**: Connected and room key ready (encryption working)

### Button States
- **"Send File..." button**: Enabled only when green (room key ready)
- Prevents sending when not connected or encryption not available

## Quick Testing

### Test Room Choice Dialog
```powershell
# Launch without arguments to see room choice
.\target\release\cliprelay-client.exe
```

### Test File Sending  
```powershell
# Launch two clients with same room code
.\target\release\cliprelay-client.exe --room-code test-files --device-name PC1
.\target\release\cliprelay-client.exe --room-code test-files --device-name PC2

# Wait for both to show GREEN tray icon
# On PC1: Double-click tray → "Send File..." → Choose file ≤5MB
# On PC2: Popup appears → Click "Save"
# File saved to Downloads\ClipRelay\
```

## Next Steps / Future Enhancements

Potential improvements (not implemented):
- [ ] File sending progress bar during large transfers
- [ ] Configurable file size limit (currently hardcoded 5MB)
- [ ] Drag-and-drop files onto send window
- [ ] File history list in UI
- [ ] Custom save location picker
- [ ] File type filters in open dialog
- [ ] Thumbnail preview for images
- [ ] Pause/resume for interrupted transfers
- [ ] Compression for large text files before encryption

---

*Last Updated: 2026-02-16*  
*Commit: 7d17fb5 - feat(client): add room choice dialog on every launch*
