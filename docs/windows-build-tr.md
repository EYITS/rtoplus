# rtoplus: Windows derleme ve kullanım

## GitHub Actions ile exe indirme

1. Fork deponuzda **Actions** sekmesini açın. GitHub fork iş akışlarını
   etkinleştirmenizi isterse etkinleştirin.
2. **Windows Build** iş akışında son başarılı çalıştırmayı açın.
3. **Artifacts → rtop-windows-x64** dosyasını indirin ve ZIP'i çıkartın.
4. İçindeki `rtop.exe` dosyasını Windows Terminal'de çalıştırın.

İş akışı her dal güncellemesinde ve pull request üzerinde otomatik çalışır.
Varsayılan dala alındıktan sonra **Run workflow** düğmesiyle elle de
başlatılabilir. Sırasıyla `cargo fmt --all --check`, Clippy, testler ve
Windows x64 MSVC release derlemesi çalışır. Son olarak `--version` ve
`--default-config` komutları sınanır. Başarısız kontrol varsa exe yayımlanmaz.
Derleme Windows üzerinde yapılır; Windows 10 ve 11 masaüstü ortamlarında
ayrı ayrı gerçek kullanım testi yapıldığı anlamına gelmez.

İndirilen exe'nin SHA256 özeti `rtop.exe.sha256` içinde bulunur. Derlenen
kaynak sürümünün commit kimliği Actions çalıştırma özetinde gösterilir.

## Uygulama grupları

- **Shift+E:** Gruplandırmayı aç/kapat.
- **Enter:** Grup satırında alt işlemleri aç/kapat; tekil işlemde ayrıntı aç.
- **e:** Mevcut Tree View görünümü.
- **Sol/sağ:** Sıralama sütununu değiştir; **r:** Sıralamayı tersine çevir.
- **f** veya **/**: Filtre. Üyelerden biri eşleşirse grubun tamamı görünür.
- **t:** Mevcut çift basış güvenliğiyle tekil işlemi sonlandır.
- **Shift+T, Shift+T:** Grup satırında ilk basış onay ister; ikinci basış,
  o anda grupta bulunan işlemleri zorla kapatır. Yeni katılan PID'ler bu
  onaya dahil edilmez. Başka bir tuş onayı iptal eder.
- **Shift+U:** GPU yüzdesini aç/kapat; GPU açıkken PID gizlenir.
- **Shift+N:** İşlem/grup başına ağ gönderme+alma hızını aç/kapat.
- **Shift+D:** İşlem/grup başına disk okuma+yazma hızını aç/kapat.
- **Sol/sağ:** Açık sütunlara göre sıralama; **r:** Sıralama yönü.

GPU, RAM'in sağında görünür. Ağ ve disk hızları byte/s olarak, kısa birimlerle
gösterilir. Üç ayar Options → proc menüsünden ve `proc_gpu`, `proc_net`,
`proc_disk` anahtarlarından bağımsız kaydedilir. Dar pencerelerde ikincil
sütunlar geçici gizlenebilir.

Ağ ve diski doğru ayırmak için yönetici olarak çalıştırmak gerekir. Kalıcı
hizmet kurulmaz; normal açılışta erişilemeyen ölçümler `--` görünür. GPU için
yönetici şartı yoktur; desteklenen WDDM sayaçları gerekir. Ağ sütunu yalnız
internet değil LAN dahil TCP/UDP trafiğini gösterir. Disk fiziksel aktarımdır;
Windows önbelleğinden karşılanan okumalar fiziksel disk trafiği sayılmaz.

Kalıcı ayar **Options → proc → proc_grouped** üzerinden veya
`%APPDATA%\rtop\rtop.toml` içinde değiştirilebilir:

```toml
proc_grouped = true
```

Gruplar executable path üzerinden belirlenir. Yol bilgisi alınamıyorsa
doğrulanabilen aynı isimli ebeveyn-çocuk ilişkileri kullanılır; sadece adları
eşleşen ilgisiz işlemler birleştirilmez. Erişim kısıtları nedeniyle bazı
işlemler ayrı görünebilir. CPU ve RAM mevcut snapshot metriklerinden
toplanır. RAM çalışma kümesi toplamıdır; paylaşılan sayfalar tekrar
sayılabileceğinden Görev Yöneticisi ile birebir eşitlik beklenmemelidir.

## Küçük pencereler ve özel yerleşim

Pencere küçülünce görünüm alanı doldurmayı sürdürür. Önce paneller
sadeleşir, işlem tablosunda ikincil sütunlar azalır. Daha dar veya kısa
pencerelerde görünür CPU, RAM ve işlem listesi öne çıkar. Pencere
büyütülünce seçilen yerleşim geri gelir. Kalıcı özel yerleşim değiştirilmez.

İstenen örnek ayar:

```toml
preset = "custom"
custom_layout = "vstack(hstack(vstack(cpu, gpu), vstack(mem, proc)), statusbar)"
```

1x1 gibi uç boyutlarda bütün panelleri göstermek hedeflenmez; yalnızca
sığan içerik çizilir. Menü ve seçenekler de küçük alanlarda sadeleşir.

## Sürekli üstte tutma

Bu sürüm rtop içine pencere sabitleme kodu eklemez. Windows Terminal'de
**Ayarlar → Görünüm → Her zaman üstte** seçeneği kullanılabilir. Bu ayar
rtop sekmesiyle birlikte bütün terminal penceresini etkiler.

Terminal `settings.json` dosyasının köküne şu ayar da eklenebilir:

```json
"alwaysOnTop": true
```

Kaynak: [Microsoft Windows Terminal görünüm ayarları](https://learn.microsoft.com/en-us/windows/terminal/customize-settings/appearance#always-on-top-mode).
PowerToys kuruluysa **Win+Ctrl+T** de kullanılabilir:
[Microsoft PowerToys Always On Top](https://learn.microsoft.com/en-us/windows/powertoys/always-on-top).

## Performans yaklaşımı

Yeni bağımlılık eklenmez. Gruplandırma var olan process snapshot üzerinden,
işlem listesi yenilendiğinde hesaplanır. Executable path aynı işlem ömrü
boyunca önbellekte tutulur. Mevcut widget yenileme aralıkları ve dirty
rendering korunur. Bu mimari tercihleri ölçülmüş CPU/RAM kazanımı olarak
yorumlamayın; gerçek tüketim karşılaştırması aynı sistem ve yük altında
ayrı ölçüm gerektirir.
