cask "voiceislocal" do
  version "1.2.0"
  sha256 "83865c90258a2dac02361e15706c7e97aa010760238738314ee791bcb7ca94da"

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
