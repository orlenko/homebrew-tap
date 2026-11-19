cask "voiceislocal" do
  version "1.0.10"
  sha256 "dd62a1c5daaa88002e048850bf4b6a4ff0ecb96c1aa3ff9182e3a29949fa917e"

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
