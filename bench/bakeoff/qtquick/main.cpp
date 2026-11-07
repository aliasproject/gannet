// Bake-off probe: Qt Quick ListView over a 100,000-row model.
#include <QGuiApplication>
#include <QAbstractListModel>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <vector>
struct M : QAbstractListModel {
  std::vector<QString> n;
  M(int c){ n.reserve(c); for(int i=0;i<c;i++) n.push_back(QString("file-%1.txt").arg(i)); }
  int rowCount(const QModelIndex&p={}) const override { return p.isValid()?0:(int)n.size(); }
  QVariant data(const QModelIndex&i,int) const override { return n[i.row()]; }
  QHash<int,QByteArray> roleNames() const override { return {{Qt::DisplayRole,"name"}}; }
};
int main(int c,char**v){ QGuiApplication a(c,v); M m(100000); QQmlApplicationEngine e; e.rootContext()->setContextProperty("files",&m);
 e.loadData(R"(import QtQuick
Window { visible:true; width:1200; height:760; color:"#111"
 ListView { anchors.fill:parent; model:files; reuseItems:true; delegate: Text { required property string name; text:name; color:"#ddd"; height:24 } } })");
 return a.exec(); }
