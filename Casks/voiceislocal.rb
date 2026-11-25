cask "voiceislocal" do
  version "1.3.0"
  sha256 "a27150ed56cd6a2d2d36689638dae095cda03ccedbe3d938f50f89bcfcc04e69"

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
