cask "voiceislocal" do
  version "1.0.8"
  sha256 "66e33a227d85bc174e2e72d5ccf8652389ad9ee73d716597e7533d4d5e1332f0"

  url "https://voiceislocal.com/downloads/VoiceIsLocal-#{version}.dmg"
  name "Voice is Local"
  desc "On-device dictation, meeting transcription and reading aloud"
  homepage "https://voiceislocal.com/"

  livecheck do
    url :homepage
    regex(/VoiceIsLocal[._-]v?(\d+(?:\.\d+)+)\.dmg/i)
  end

  auto_updates true
  depends_on arch: :arm64
  depends_on macos: :golden_gate

  app "VoiceIsLocal.app"
  binary "#{appdir}/VoiceIsLocal.app/Contents/MacOS/voiceislocal"

  zap trash: [
    "~/Library/Application Support/Holos",
    "~/Library/Preferences/ca.orlenko.holos.app.plist",
  ]
end
