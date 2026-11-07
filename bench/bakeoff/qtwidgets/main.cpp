// Bake-off probe: QTreeView over a 100,000-row model, raster painting.
#include <QApplication>
#include <QAbstractTableModel>
#include <QTreeView>
#include <vector>
#include <string>
struct M : QAbstractTableModel {
  std::vector<QString> n;
  M(int c){ n.reserve(c); for(int i=0;i<c;i++) n.push_back(QString("file-%1.txt").arg(i)); }
  int rowCount(const QModelIndex&p={}) const override { return p.isValid()?0:(int)n.size(); }
  int columnCount(const QModelIndex&p={}) const override { return p.isValid()?0:3; }
  QVariant data(const QModelIndex&i,int r) const override { if(r!=Qt::DisplayRole) return {}; return i.column()==0?QVariant(n[i.row()]):QVariant(QStringLiteral("x")); }
};
int main(int c,char**v){ QApplication a(c,v); M m(100000); QTreeView t; t.setUniformRowHeights(true); t.setRootIsDecorated(false); t.setModel(&m); t.resize(1200,760); t.show(); return a.exec(); }
